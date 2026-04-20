//! Singleton GitHub-org binding for a Djinn deployment (Phase 1).
//!
//! This deployment is locked to exactly one GitHub org. `org_config` is a
//! single-row table (enforced by `CHECK (id = 1)` + PK on `id`) that records
//! which org, which GitHub App, and which installation grants server-side
//! access. Phase 2 wires `auth.rs` to reject logins from non-members.
//!
//! The row is written once at deployment setup and then read-only for the
//! life of the deployment. `set_once` fails loudly on a second attempt — we
//! never silently rebind.

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::database::Database;
use crate::error::DbError;

/// Row materialised from the `org_config` table.
#[derive(Debug, Clone, Serialize, Deserialize, sqlx::FromRow)]
pub struct OrgConfig {
    pub id: i32,
    pub github_org_id: i64,
    pub github_org_login: String,
    pub app_id: i64,
    pub installation_id: i64,
    pub created_at: String,
}

/// Input for [`OrgConfigRepository::set_once`].
#[derive(Debug, Clone)]
pub struct NewOrgConfig<'a> {
    pub github_org_id: i64,
    pub github_org_login: &'a str,
    pub app_id: i64,
    pub installation_id: i64,
}

#[derive(Clone)]
pub struct OrgConfigRepository {
    db: Database,
}

impl OrgConfigRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Return the singleton row if set, or `None` if this deployment has not
    /// yet been bound to an org.
    pub async fn get(&self) -> Result<Option<OrgConfig>> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_as!(
            OrgConfig,
            "SELECT id, github_org_id, github_org_login, app_id, installation_id, created_at
             FROM org_config WHERE id = 1",
        )
        .fetch_optional(self.db.pool())
        .await?)
    }

    /// Insert the singleton row. If a row already exists, returns
    /// `DbError::InvalidTransition` — we never silently overwrite a
    /// deployment's org binding.
    pub async fn set_once(&self, cfg: NewOrgConfig<'_>) -> Result<OrgConfig> {
        self.db.ensure_initialized().await?;

        if self.get().await?.is_some() {
            return Err(DbError::InvalidTransition(
                "org_config already set; this deployment is already bound to a GitHub org"
                    .to_owned(),
            ));
        }

        sqlx::query!(
            "INSERT INTO org_config
                (id, github_org_id, github_org_login, app_id, installation_id)
             VALUES (1, ?, ?, ?, ?)",
            cfg.github_org_id,
            cfg.github_org_login,
            cfg.app_id,
            cfg.installation_id,
        )
        .execute(self.db.pool())
        .await?;

        let row = sqlx::query_as!(
            OrgConfig,
            "SELECT id, github_org_id, github_org_login, app_id, installation_id, created_at
             FROM org_config WHERE id = 1",
        )
        .fetch_one(self.db.pool())
        .await?;

        Ok(row)
    }

    /// Insert or replace the singleton row.
    ///
    /// Loosened counterpart to [`set_once`](Self::set_once), introduced to
    /// back the in-UI installation picker: an operator setting up a fresh
    /// deployment may pick the wrong installation on the first click and
    /// reasonably expect a second click to overwrite the binding. The
    /// `set_once` strict invariant remains in force for the legacy
    /// `app_setup_callback` flow, which trusts GitHub's redirect to be the
    /// authoritative one-shot.
    ///
    /// The `created_at` of an overwriting row reflects the *latest* bind —
    /// callers that need provenance for the original bind should snapshot
    /// the row before invoking this.
    pub async fn set_or_replace(&self, cfg: NewOrgConfig<'_>) -> Result<OrgConfig> {
        self.db.ensure_initialized().await?;

        // Two-step replace so we don't depend on dialect-specific UPSERT.
        // The row id is hard-coded to 1 by the singleton invariant.
        sqlx::query!("DELETE FROM org_config WHERE id = 1")
            .execute(self.db.pool())
            .await?;

        sqlx::query!(
            "INSERT INTO org_config
                (id, github_org_id, github_org_login, app_id, installation_id)
             VALUES (1, ?, ?, ?, ?)",
            cfg.github_org_id,
            cfg.github_org_login,
            cfg.app_id,
            cfg.installation_id,
        )
        .execute(self.db.pool())
        .await?;

        let row = sqlx::query_as!(
            OrgConfig,
            "SELECT id, github_org_id, github_org_login, app_id, installation_id, created_at
             FROM org_config WHERE id = 1",
        )
        .fetch_one(self.db.pool())
        .await?;

        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_db() -> Database {
        Database::open_in_memory().unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_returns_none_when_unset() {
        let repo = OrgConfigRepository::new(test_db());
        let row = repo.get().await.unwrap();
        assert!(row.is_none());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_once_inserts_then_get_returns_row() {
        let repo = OrgConfigRepository::new(test_db());

        let created = repo
            .set_once(NewOrgConfig {
                github_org_id: 42,
                github_org_login: "acme-corp",
                app_id: 100,
                installation_id: 200,
            })
            .await
            .unwrap();
        assert_eq!(created.id, 1);
        assert_eq!(created.github_org_id, 42);
        assert_eq!(created.github_org_login, "acme-corp");
        assert_eq!(created.app_id, 100);
        assert_eq!(created.installation_id, 200);

        let fetched = repo.get().await.unwrap().unwrap();
        assert_eq!(fetched.github_org_login, "acme-corp");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_once_errors_on_second_call() {
        let repo = OrgConfigRepository::new(test_db());

        repo.set_once(NewOrgConfig {
            github_org_id: 1,
            github_org_login: "first",
            app_id: 10,
            installation_id: 20,
        })
        .await
        .unwrap();

        let err = repo
            .set_once(NewOrgConfig {
                github_org_id: 2,
                github_org_login: "second",
                app_id: 30,
                installation_id: 40,
            })
            .await
            .expect_err("second set_once must fail loudly");

        match err {
            DbError::InvalidTransition(msg) => assert!(msg.contains("already set")),
            other => panic!("expected InvalidTransition, got {other:?}"),
        }

        // And the first binding must still be intact.
        let fetched = repo.get().await.unwrap().unwrap();
        assert_eq!(fetched.github_org_login, "first");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_or_replace_overwrites_existing_row() {
        let repo = OrgConfigRepository::new(test_db());

        // Initial bind via set_once.
        repo.set_once(NewOrgConfig {
            github_org_id: 1,
            github_org_login: "first",
            app_id: 10,
            installation_id: 20,
        })
        .await
        .unwrap();

        // Re-bind via set_or_replace.
        let replaced = repo
            .set_or_replace(NewOrgConfig {
                github_org_id: 2,
                github_org_login: "second",
                app_id: 30,
                installation_id: 40,
            })
            .await
            .unwrap();
        assert_eq!(replaced.id, 1);
        assert_eq!(replaced.github_org_login, "second");
        assert_eq!(replaced.installation_id, 40);

        let fetched = repo.get().await.unwrap().unwrap();
        assert_eq!(fetched.github_org_login, "second");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_or_replace_inserts_when_unset() {
        let repo = OrgConfigRepository::new(test_db());
        let row = repo
            .set_or_replace(NewOrgConfig {
                github_org_id: 99,
                github_org_login: "fresh",
                app_id: 1,
                installation_id: 2,
            })
            .await
            .unwrap();
        assert_eq!(row.github_org_login, "fresh");
    }
}
