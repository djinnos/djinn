//! Singleton GitHub-org binding for a Djinn deployment.
//!
//! This deployment is locked to exactly one GitHub org. `org_config` is a
//! single-row table (enforced by `CHECK (id = 1)` + PK on `id`) that records
//! which org, which GitHub App, and which installation grants server-side
//! access. `auth.rs` rejects logins from non-members.
//!
//! The row is written by the in-UI installation picker (see
//! `server/src/server/github_install.rs`) and by the GitHub App-install
//! redirect callback (`server/src/server/auth.rs::app_setup_callback`),
//! both of which use the repository writers below. Bootstrap callers use
//! [`OrgConfigRepository::create_if_absent`] so concurrent setup requests
//! cannot replace an established binding. [`OrgConfigRepository::set`] is
//! retained for explicit operator-controlled replacement paths.

use serde::{Deserialize, Serialize};

use crate::Result;
use crate::database::Database;

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

/// Input for [`OrgConfigRepository::create_if_absent`] and
/// [`OrgConfigRepository::set`].
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

    /// Create the singleton org binding only when it does not already exist.
    ///
    /// Returns `Some(row)` when this call created the binding and `None` when
    /// another setup request (or an earlier setup) already owns the singleton
    /// row. The conflict check and insert are one database statement, so two
    /// concurrent bootstrap requests cannot overwrite each other.
    pub async fn create_if_absent(&self, cfg: NewOrgConfig<'_>) -> Result<Option<OrgConfig>> {
        self.db.ensure_initialized().await?;

        let inserted = sqlx::query_as::<_, OrgConfig>(
            "INSERT INTO org_config
                (id, github_org_id, github_org_login, app_id, installation_id)
             VALUES (1, $1, $2, $3, $4)
             ON CONFLICT (id) DO NOTHING
             RETURNING id, github_org_id, github_org_login, app_id, installation_id, created_at",
        )
        .bind(cfg.github_org_id)
        .bind(cfg.github_org_login)
        .bind(cfg.app_id)
        .bind(cfg.installation_id)
        .fetch_optional(self.db.pool())
        .await?;

        Ok(inserted)
    }

    /// Insert or replace the singleton org-binding row.
    ///
    /// This explicit replacement API is for operator-controlled repair or
    /// migration paths. Public bootstrap surfaces must use
    /// [`Self::create_if_absent`] so they cannot replace a completed binding.
    ///
    /// The `created_at` of an overwriting row reflects the *latest* bind —
    /// callers that need provenance for the original bind should snapshot
    /// the row before invoking this.
    pub async fn set(&self, cfg: NewOrgConfig<'_>) -> Result<OrgConfig> {
        self.db.ensure_initialized().await?;

        // Two-step replace so we don't depend on dialect-specific UPSERT.
        // The row id is hard-coded to 1 by the singleton invariant.
        sqlx::query!("DELETE FROM org_config WHERE id = 1")
            .execute(self.db.pool())
            .await?;

        sqlx::query!(
            "INSERT INTO org_config
                (id, github_org_id, github_org_login, app_id, installation_id)
             VALUES (1, $1, $2, $3, $4)",
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
    async fn set_inserts_then_get_returns_row() {
        let repo = OrgConfigRepository::new(test_db());

        let created = repo
            .set(NewOrgConfig {
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
    async fn set_overwrites_existing_row() {
        let repo = OrgConfigRepository::new(test_db());

        // Initial bind.
        repo.set(NewOrgConfig {
            github_org_id: 1,
            github_org_login: "first",
            app_id: 10,
            installation_id: 20,
        })
        .await
        .unwrap();

        // Re-bind to a different installation.
        let replaced = repo
            .set(NewOrgConfig {
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
    async fn create_if_absent_does_not_replace_existing_row() {
        let repo = OrgConfigRepository::new(test_db());

        let created = repo
            .create_if_absent(NewOrgConfig {
                github_org_id: 1,
                github_org_login: "first",
                app_id: 10,
                installation_id: 20,
            })
            .await
            .unwrap();
        assert_eq!(created.unwrap().github_org_login, "first");

        let conflict = repo
            .create_if_absent(NewOrgConfig {
                github_org_id: 2,
                github_org_login: "second",
                app_id: 30,
                installation_id: 40,
            })
            .await
            .unwrap();
        assert!(conflict.is_none());

        let fetched = repo.get().await.unwrap().unwrap();
        assert_eq!(fetched.github_org_login, "first");
        assert_eq!(fetched.installation_id, 20);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn concurrent_create_if_absent_has_exactly_one_winner() {
        let repo = OrgConfigRepository::new(test_db());
        let first_repo = repo.clone();
        let second_repo = repo.clone();

        let (first, second) = tokio::join!(
            first_repo.create_if_absent(NewOrgConfig {
                github_org_id: 1,
                github_org_login: "first",
                app_id: 10,
                installation_id: 20,
            }),
            second_repo.create_if_absent(NewOrgConfig {
                github_org_id: 2,
                github_org_login: "second",
                app_id: 30,
                installation_id: 40,
            }),
        );

        let winners = [first.unwrap(), second.unwrap()]
            .into_iter()
            .filter(Option::is_some)
            .count();
        assert_eq!(winners, 1);

        let fetched = repo.get().await.unwrap().unwrap();
        assert!(matches!(
            fetched.github_org_login.as_str(),
            "first" | "second"
        ));
    }
}
