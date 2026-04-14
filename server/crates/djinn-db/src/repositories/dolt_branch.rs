use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::{Database, DatabaseBackendKind, Error, MysqlBackendFlavor, task_branch_name};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoltBranchLifecycleAction {
    Create,
    Checkout,
    Merge,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoltBranchLifecycleResult {
    pub action: DoltBranchLifecycleAction,
    pub branch: String,
    pub base_branch: Option<String>,
    pub merged_into: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum DoltBranchError {
    #[error("database backend `{backend}` does not support Dolt branch lifecycle SQL")]
    UnsupportedBackend { backend: String },
    #[error("dolt branch sql failed during {action}: {message}")]
    Sql {
        action: &'static str,
        message: String,
    },
    #[error(transparent)]
    Db(#[from] Error),
}

#[async_trait]
pub trait DoltBranchLifecycle {
    async fn create_branch(
        &self,
        branch: &str,
        from_branch: &str,
    ) -> std::result::Result<DoltBranchLifecycleResult, DoltBranchError>;
    async fn checkout_branch(
        &self,
        branch: &str,
    ) -> std::result::Result<DoltBranchLifecycleResult, DoltBranchError>;
    async fn merge_branch(
        &self,
        source_branch: &str,
        into_branch: &str,
    ) -> std::result::Result<DoltBranchLifecycleResult, DoltBranchError>;
    async fn delete_branch(
        &self,
        branch: &str,
    ) -> std::result::Result<DoltBranchLifecycleResult, DoltBranchError>;
}

pub struct DoltBranchSqlHelper<'a> {
    db: &'a Database,
}

impl<'a> DoltBranchSqlHelper<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn task_branch(task_short_id: &str) -> String {
        task_branch_name(task_short_id)
    }

    fn ensure_dolt_backend(&self) -> std::result::Result<(), DoltBranchError> {
        let capabilities = self.db.backend_capabilities();
        let is_dolt = capabilities.backend_kind == DatabaseBackendKind::Mysql
            && capabilities.supports_branching_metadata
            && capabilities.backend_label == MysqlBackendFlavor::Dolt.as_str();
        if is_dolt {
            Ok(())
        } else {
            Err(DoltBranchError::UnsupportedBackend {
                backend: capabilities.backend_label.clone(),
            })
        }
    }

    async fn exec_scalar_string(
        &self,
        sql: &str,
        binds: &[&str],
        action: &'static str,
    ) -> std::result::Result<(), DoltBranchError> {
        self.ensure_dolt_backend()?;
        let Some(pool) = self.db.mysql_pool() else {
            return Err(DoltBranchError::UnsupportedBackend {
                backend: self.db.bootstrap_info().backend_label.clone(),
            });
        };
        let mut query = sqlx::query(sql);
        for bind in binds {
            query = query.bind(*bind);
        }
        query
            .execute(pool)
            .await
            .map_err(|err| DoltBranchError::Sql {
                action,
                message: err.to_string(),
            })?;
        Ok(())
    }

    pub async fn branch_exists(&self, branch: &str) -> std::result::Result<bool, DoltBranchError> {
        self.ensure_dolt_backend()?;
        let Some(pool) = self.db.mysql_pool() else {
            return Err(DoltBranchError::UnsupportedBackend {
                backend: self.db.bootstrap_info().backend_label.clone(),
            });
        };
        let row = sqlx::query("SELECT COUNT(*) AS count FROM dolt_branches WHERE name = ?")
            .bind(branch)
            .fetch_one(pool)
            .await
            .map_err(|err| DoltBranchError::Sql {
                action: "branch_exists",
                message: err.to_string(),
            })?;
        let count: i64 = row.try_get("count").map_err(|err| DoltBranchError::Sql {
            action: "branch_exists",
            message: err.to_string(),
        })?;
        Ok(count > 0)
    }
}

#[async_trait]
impl DoltBranchLifecycle for DoltBranchSqlHelper<'_> {
    async fn create_branch(
        &self,
        branch: &str,
        from_branch: &str,
    ) -> std::result::Result<DoltBranchLifecycleResult, DoltBranchError> {
        self.exec_scalar_string(
            "CALL DOLT_BRANCH(?, ?)",
            &[branch, from_branch],
            "create_branch",
        )
        .await?;
        Ok(DoltBranchLifecycleResult {
            action: DoltBranchLifecycleAction::Create,
            branch: branch.to_string(),
            base_branch: Some(from_branch.to_string()),
            merged_into: None,
        })
    }

    async fn checkout_branch(
        &self,
        branch: &str,
    ) -> std::result::Result<DoltBranchLifecycleResult, DoltBranchError> {
        self.exec_scalar_string("CALL DOLT_CHECKOUT(?)", &[branch], "checkout_branch")
            .await?;
        Ok(DoltBranchLifecycleResult {
            action: DoltBranchLifecycleAction::Checkout,
            branch: branch.to_string(),
            base_branch: None,
            merged_into: None,
        })
    }

    async fn merge_branch(
        &self,
        source_branch: &str,
        into_branch: &str,
    ) -> std::result::Result<DoltBranchLifecycleResult, DoltBranchError> {
        self.checkout_branch(into_branch).await?;
        self.exec_scalar_string("CALL DOLT_MERGE(?)", &[source_branch], "merge_branch")
            .await?;
        Ok(DoltBranchLifecycleResult {
            action: DoltBranchLifecycleAction::Merge,
            branch: source_branch.to_string(),
            base_branch: None,
            merged_into: Some(into_branch.to_string()),
        })
    }

    async fn delete_branch(
        &self,
        branch: &str,
    ) -> std::result::Result<DoltBranchLifecycleResult, DoltBranchError> {
        self.exec_scalar_string("CALL DOLT_BRANCH('-d', ?)", &[branch], "delete_branch")
            .await?;
        Ok(DoltBranchLifecycleResult {
            action: DoltBranchLifecycleAction::Delete,
            branch: branch.to_string(),
            base_branch: None,
            merged_into: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DatabaseConnectConfig, MysqlDatabaseConfig};

    #[test]
    fn task_branch_uses_existing_contract() {
        assert_eq!(DoltBranchSqlHelper::task_branch("he6k"), "task/he6k");
    }

    #[tokio::test]
    async fn sqlite_backend_is_rejected() {
        let db = Database::open_in_memory().unwrap();
        let helper = DoltBranchSqlHelper::new(&db);
        let error = helper.create_branch("task/he6k", "main").await.unwrap_err();
        assert!(matches!(error, DoltBranchError::UnsupportedBackend { .. }));
    }

    #[tokio::test]
    async fn mysql_dolt_capability_detected_from_backend_metadata() {
        let db = Database::open_with_config(DatabaseConnectConfig::Mysql(MysqlDatabaseConfig {
            url: "mysql://root@127.0.0.1:3306/djinn".to_string(),
            flavor: MysqlBackendFlavor::Dolt,
        }))
        .unwrap();
        let helper = DoltBranchSqlHelper::new(&db);
        assert!(helper.ensure_dolt_backend().is_ok());
    }
}
