use std::collections::HashMap;

use djinn_core::events::EventBus;
use djinn_core::models::{DispatchPause, DispatchPauseScope, DispatchPauseState, DjinnSettings};
use sqlx::Row;

use crate::Result;
use crate::database::Database;
use crate::error::DbError;
use crate::repositories::settings::SettingsRepository;

const SETTINGS_RAW_KEY: &str = "settings.raw";

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchPauseTarget {
    Global,
    Project(String),
    User(String),
}

impl DispatchPauseTarget {
    pub fn global() -> Self {
        Self::Global
    }

    pub fn project(project_id: impl Into<String>) -> Self {
        Self::Project(project_id.into())
    }

    pub fn user(user_id: impl Into<String>) -> Self {
        Self::User(user_id.into())
    }

    pub fn scope(&self) -> DispatchPauseScope {
        match self {
            Self::Global => DispatchPauseScope::Global,
            Self::Project(_) => DispatchPauseScope::Project,
            Self::User(_) => DispatchPauseScope::User,
        }
    }

    pub fn target_id(&self) -> Option<&str> {
        match self {
            Self::Global => None,
            Self::Project(project_id) | Self::User(project_id) => Some(project_id.as_str()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DispatchPauseMutation {
    pub scope: DispatchPauseScope,
    pub target_id: Option<String>,
    pub previous: Option<DispatchPause>,
    pub current: Option<DispatchPause>,
    pub changed: bool,
}

pub struct DispatchPauseRepository {
    db: Database,
    settings: SettingsRepository,
}

impl DispatchPauseRepository {
    pub fn new(db: Database, events: EventBus) -> Self {
        Self {
            db: db.clone(),
            settings: SettingsRepository::new(db, events),
        }
    }

    /// Return the full pause-state snapshot without mutating persisted state.
    pub async fn get_status(&self) -> Result<DispatchPauseState> {
        self.db.ensure_initialized().await?;
        let global = self.get_global().await?;
        let rows = sqlx::query(
            r#"SELECT scope, target_id, paused_by, paused_at, reason, expires_at
               FROM dispatch_pauses
               ORDER BY scope ASC, target_id ASC"#,
        )
        .fetch_all(self.db.pool())
        .await?;

        let mut projects = HashMap::new();
        let mut users = HashMap::new();
        for row in rows {
            let scope: String = row.try_get("scope")?;
            let target_id: String = row.try_get("target_id")?;
            let pause = pause_from_row(&row)?;
            match scope.as_str() {
                "project" => {
                    projects.insert(target_id, pause);
                }
                "user" => {
                    users.insert(target_id, pause);
                }
                _ => {}
            }
        }

        Ok(DispatchPauseState {
            global,
            projects,
            users,
        })
    }

    /// Return one scope's pause metadata without mutating persisted state.
    pub async fn get_scope(&self, target: DispatchPauseTarget) -> Result<Option<DispatchPause>> {
        validate_target(&target)?;
        match target {
            DispatchPauseTarget::Global => self.get_global().await,
            DispatchPauseTarget::Project(project_id) => {
                self.get_scoped(DispatchPauseScope::Project, &project_id)
                    .await
            }
            DispatchPauseTarget::User(user_id) => {
                self.get_scoped(DispatchPauseScope::User, &user_id).await
            }
        }
    }

    pub async fn pause(
        &self,
        target: DispatchPauseTarget,
        pause: DispatchPause,
    ) -> Result<DispatchPauseMutation> {
        validate_target(&target)?;
        match target {
            DispatchPauseTarget::Global => self.pause_global(pause).await,
            DispatchPauseTarget::Project(project_id) => {
                self.pause_scoped(DispatchPauseScope::Project, project_id, pause)
                    .await
            }
            DispatchPauseTarget::User(user_id) => {
                self.pause_scoped(DispatchPauseScope::User, user_id, pause)
                    .await
            }
        }
    }

    pub async fn resume(&self, target: DispatchPauseTarget) -> Result<DispatchPauseMutation> {
        validate_target(&target)?;
        match target {
            DispatchPauseTarget::Global => self.resume_global().await,
            DispatchPauseTarget::Project(project_id) => {
                self.resume_scoped(DispatchPauseScope::Project, project_id)
                    .await
            }
            DispatchPauseTarget::User(user_id) => {
                self.resume_scoped(DispatchPauseScope::User, user_id).await
            }
        }
    }

    async fn get_global(&self) -> Result<Option<DispatchPause>> {
        let settings = self.load_settings().await?;
        Ok(settings.dispatch_pause)
    }

    async fn pause_global(&self, pause: DispatchPause) -> Result<DispatchPauseMutation> {
        let mut settings = self.load_settings().await?;
        let previous = settings.dispatch_pause.clone();
        if previous.is_some() {
            return Ok(DispatchPauseMutation {
                scope: DispatchPauseScope::Global,
                target_id: None,
                current: previous.clone(),
                previous,
                changed: false,
            });
        }

        settings.dispatch_pause = Some(pause.clone());
        self.persist_settings(&settings).await?;
        Ok(DispatchPauseMutation {
            scope: DispatchPauseScope::Global,
            target_id: None,
            previous: None,
            current: Some(pause),
            changed: true,
        })
    }

    async fn resume_global(&self) -> Result<DispatchPauseMutation> {
        let mut settings = self.load_settings().await?;
        let previous = settings.dispatch_pause.take();
        if previous.is_none() {
            return Ok(DispatchPauseMutation {
                scope: DispatchPauseScope::Global,
                target_id: None,
                previous: None,
                current: None,
                changed: false,
            });
        }

        self.persist_settings(&settings).await?;
        Ok(DispatchPauseMutation {
            scope: DispatchPauseScope::Global,
            target_id: None,
            previous,
            current: None,
            changed: true,
        })
    }

    async fn load_settings(&self) -> Result<DjinnSettings> {
        Ok(self
            .settings
            .get(SETTINGS_RAW_KEY)
            .await?
            .map(|setting| DjinnSettings::from_db_value(&setting.value))
            .unwrap_or_default())
    }

    async fn persist_settings(&self, settings: &DjinnSettings) -> Result<()> {
        let raw = serde_json::to_string(settings)?;
        self.settings.set(SETTINGS_RAW_KEY, &raw).await?;
        Ok(())
    }

    async fn get_scoped(
        &self,
        scope: DispatchPauseScope,
        target_id: &str,
    ) -> Result<Option<DispatchPause>> {
        self.db.ensure_initialized().await?;
        let row = sqlx::query(
            r#"SELECT paused_by, paused_at, reason, expires_at
               FROM dispatch_pauses
               WHERE scope = $1 AND target_id = $2"#,
        )
        .bind(scope.as_str())
        .bind(target_id)
        .fetch_optional(self.db.pool())
        .await?;
        row.as_ref().map(pause_from_row).transpose()
    }

    async fn pause_scoped(
        &self,
        scope: DispatchPauseScope,
        target_id: String,
        pause: DispatchPause,
    ) -> Result<DispatchPauseMutation> {
        self.db.ensure_initialized().await?;
        let result = sqlx::query(
            r#"INSERT INTO dispatch_pauses
                  (scope, target_id, paused_by, paused_at, reason, expires_at)
               VALUES ($1, $2, $3, $4, $5, $6)
               ON CONFLICT (scope, target_id) DO NOTHING"#,
        )
        .bind(scope.as_str())
        .bind(&target_id)
        .bind(&pause.paused_by)
        .bind(&pause.paused_at)
        .bind(&pause.reason)
        .bind(&pause.expires_at)
        .execute(self.db.pool())
        .await?;

        if result.rows_affected() == 0 {
            let current = self.get_scoped(scope, &target_id).await?;
            return Ok(DispatchPauseMutation {
                scope,
                target_id: Some(target_id),
                previous: current.clone(),
                current,
                changed: false,
            });
        }

        Ok(DispatchPauseMutation {
            scope,
            target_id: Some(target_id),
            previous: None,
            current: Some(pause),
            changed: true,
        })
    }

    async fn resume_scoped(
        &self,
        scope: DispatchPauseScope,
        target_id: String,
    ) -> Result<DispatchPauseMutation> {
        self.db.ensure_initialized().await?;
        let previous = self.get_scoped(scope, &target_id).await?;
        if previous.is_none() {
            return Ok(DispatchPauseMutation {
                scope,
                target_id: Some(target_id),
                previous: None,
                current: None,
                changed: false,
            });
        }

        sqlx::query("DELETE FROM dispatch_pauses WHERE scope = $1 AND target_id = $2")
            .bind(scope.as_str())
            .bind(&target_id)
            .execute(self.db.pool())
            .await?;

        Ok(DispatchPauseMutation {
            scope,
            target_id: Some(target_id),
            previous,
            current: None,
            changed: true,
        })
    }
}

fn validate_target(target: &DispatchPauseTarget) -> Result<()> {
    match target {
        DispatchPauseTarget::Global => Ok(()),
        DispatchPauseTarget::Project(project_id) | DispatchPauseTarget::User(project_id)
            if project_id.trim().is_empty() =>
        {
            Err(DbError::InvalidData(
                "dispatch pause target id must not be empty".to_owned(),
            ))
        }
        DispatchPauseTarget::Project(_) | DispatchPauseTarget::User(_) => Ok(()),
    }
}

fn pause_from_row(row: &sqlx::postgres::PgRow) -> Result<DispatchPause> {
    Ok(DispatchPause {
        paused_by: row.try_get("paused_by")?,
        paused_at: row.try_get("paused_at")?,
        reason: row.try_get("reason")?,
        expires_at: row.try_get("expires_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    fn pause_by(actor: &str, reason: &str) -> DispatchPause {
        DispatchPause {
            paused_by: actor.to_owned(),
            paused_at: "2026-06-12T00:00:00.000Z".to_owned(),
            reason: reason.to_owned(),
            expires_at: None,
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn global_pause_resume_and_noop_are_persisted_in_settings() {
        let db = test_db();
        let repo = DispatchPauseRepository::new(db.clone(), EventBus::noop());

        assert!(
            repo.get_scope(DispatchPauseTarget::global())
                .await
                .unwrap()
                .is_none()
        );

        let first = pause_by("admin", "maintenance");
        let paused = repo
            .pause(DispatchPauseTarget::global(), first.clone())
            .await
            .unwrap();
        assert!(paused.changed);
        assert_eq!(paused.current.as_ref(), Some(&first));

        let second = pause_by("other", "do not overwrite");
        let noop = repo
            .pause(DispatchPauseTarget::global(), second)
            .await
            .unwrap();
        assert!(!noop.changed);
        assert_eq!(noop.current.as_ref(), Some(&first));

        let fresh_repo = DispatchPauseRepository::new(db, EventBus::noop());
        assert_eq!(
            fresh_repo
                .get_scope(DispatchPauseTarget::global())
                .await
                .unwrap(),
            Some(first.clone())
        );

        let resumed = fresh_repo
            .resume(DispatchPauseTarget::global())
            .await
            .unwrap();
        assert!(resumed.changed);
        assert_eq!(resumed.previous.as_ref(), Some(&first));
        assert!(resumed.current.is_none());

        let noop_resume = fresh_repo
            .resume(DispatchPauseTarget::global())
            .await
            .unwrap();
        assert!(!noop_resume.changed);
        assert!(noop_resume.previous.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn project_pause_resume_noop_and_status_survive_fresh_repository() {
        let db = test_db();
        let repo = DispatchPauseRepository::new(db.clone(), EventBus::noop());
        let pause = pause_by("lead", "stop project");

        let result = repo
            .pause(DispatchPauseTarget::project("project-1"), pause.clone())
            .await
            .unwrap();
        assert!(result.changed);

        let duplicate = repo
            .pause(
                DispatchPauseTarget::project("project-1"),
                pause_by("lead2", "new reason"),
            )
            .await
            .unwrap();
        assert!(!duplicate.changed);
        assert_eq!(duplicate.current.as_ref(), Some(&pause));

        let fresh_repo = DispatchPauseRepository::new(db, EventBus::noop());
        let status = fresh_repo.get_status().await.unwrap();
        assert_eq!(status.projects.get("project-1"), Some(&pause));
        assert!(status.users.is_empty());

        let resumed = fresh_repo
            .resume(DispatchPauseTarget::project("project-1"))
            .await
            .unwrap();
        assert!(resumed.changed);
        assert_eq!(resumed.previous.as_ref(), Some(&pause));

        let noop = fresh_repo
            .resume(DispatchPauseTarget::project("project-1"))
            .await
            .unwrap();
        assert!(!noop.changed);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn user_pause_resume_noop_and_status_survive_fresh_repository() {
        let db = test_db();
        let repo = DispatchPauseRepository::new(db.clone(), EventBus::noop());
        let pause = pause_by("lead", "stop user");

        repo.pause(DispatchPauseTarget::user("user-1"), pause.clone())
            .await
            .unwrap();

        let fresh_repo = DispatchPauseRepository::new(db, EventBus::noop());
        let status = fresh_repo.get_status().await.unwrap();
        assert_eq!(status.users.get("user-1"), Some(&pause));
        assert!(status.projects.is_empty());

        let resumed = fresh_repo
            .resume(DispatchPauseTarget::user("user-1"))
            .await
            .unwrap();
        assert!(resumed.changed);

        let noop = fresh_repo
            .resume(DispatchPauseTarget::user("user-1"))
            .await
            .unwrap();
        assert!(!noop.changed);
    }
}
