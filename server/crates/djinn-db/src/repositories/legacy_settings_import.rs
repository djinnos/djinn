//! Atomic one-release import for the retired project-local settings document.

use std::collections::BTreeMap;

use djinn_stack::environment::{EnvironmentConfig, HookCommand};
use serde::Deserialize;
use serde_json::Value;
use thiserror::Error;

use crate::Database;

const FAMILY: &str = "legacy_settings";
const RELEASE: &str = "qiy6-r7";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacySettingsImportResult {
    Imported,
    AlreadyImported,
}

#[derive(Debug, Error)]
pub enum LegacySettingsImportError {
    #[error("legacy settings JSON is invalid: {0}")]
    Parse(#[from] serde_json::Error),
    #[error("legacy settings has unsupported semantics: {0}")]
    Unsupported(String),
    #[error("legacy settings conflicts with existing DB configuration: {0}")]
    Conflict(String),
    #[error("legacy settings database operation failed: {0}")]
    Database(#[from] crate::Error),
    #[error("legacy settings SQL operation failed: {0}")]
    Sqlx(#[from] sqlx::Error),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacySettings {
    #[serde(default)]
    agent_mcp_defaults: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    global_skills: Vec<String>,
    #[serde(default)]
    setup: Vec<String>,
}

/// The workspace layer owns source-file deletion and calls this first. It only
/// deletes after `Ok`, so every parse/conflict/database failure retains source.
#[derive(Clone)]
pub struct LegacySettingsImport {
    db: Database,
}

impl LegacySettingsImport {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn import(
        &self,
        project_id: &str,
        source: &[u8],
    ) -> Result<LegacySettingsImportResult, LegacySettingsImportError> {
        self.db.ensure_initialized().await?;
        let settings = match serde_json::from_slice::<LegacySettings>(source) {
            Ok(settings) => settings,
            Err(error) => {
                self.record_failure(project_id, &format!("unparseable input: {error}"))
                    .await?;
                return Err(error.into());
            }
        };
        let semantic_error = if !settings.global_skills.is_empty() {
            Some("global_skills has no direct cut-over target".to_string())
        } else if settings.agent_mcp_defaults.iter().any(|(role, servers)| {
            role.trim().is_empty() || servers.iter().any(|server| server.trim().is_empty())
        }) {
            Some("agent_mcp_defaults contains an empty role or server name".to_string())
        } else if settings.setup.iter().any(|hook| hook.is_empty()) {
            Some("setup contains an empty hook".to_string())
        } else {
            None
        };
        if let Some(detail) = semantic_error {
            self.record_failure(project_id, &detail).await?;
            return Err(LegacySettingsImportError::Unsupported(detail));
        }

        // Mutation rolls back before durable failure recording.
        let outcome = async {
            let mut tx = self.db.pool().begin().await?;
        let prior: Option<String> = sqlx::query_scalar(
            "SELECT result FROM project_live_state_migrations WHERE project_id=$1 AND family=$2 AND release=$3 FOR UPDATE",
        )
        .bind(project_id)
        .bind(FAMILY)
        .bind(RELEASE)
        .fetch_optional(&mut *tx)
        .await?;
        if prior.as_deref() == Some("succeeded") {
            tx.commit().await?;
            return Ok(LegacySettingsImportResult::AlreadyImported);
        }
        let raw: Option<Value> =
            sqlx::query_scalar("SELECT environment_config FROM projects WHERE id=$1 FOR UPDATE")
                .bind(project_id)
                .fetch_optional(&mut *tx)
                .await?;
        let raw = raw.ok_or_else(|| {
            LegacySettingsImportError::Conflict(format!("unknown project {project_id}"))
        })?;
        let mut config = if raw == serde_json::json!({}) {
            EnvironmentConfig::empty()
        } else {
            serde_json::from_value(raw).map_err(|error| {
                LegacySettingsImportError::Conflict(format!(
                    "existing environment_config is invalid: {error}"
                ))
            })?
        };
        let hooks: Vec<HookCommand> = settings
            .setup
            .iter()
            .cloned()
            .map(HookCommand::Shell)
            .collect();
        let conflict = (!config.agent_mcp_defaults.is_empty()
            && config.agent_mcp_defaults != settings.agent_mcp_defaults)
            .then_some("agent_mcp_defaults already has a different DB value")
            .or_else(|| {
                (!config.lifecycle.pre_verification.is_empty()
                    && config.lifecycle.pre_verification != hooks)
                    .then_some("lifecycle.pre_verification already has a different DB value")
            });
        if let Some(detail) = conflict {
            return Err(LegacySettingsImportError::Conflict(detail.into()));
        }
        config.agent_mcp_defaults = settings.agent_mcp_defaults;
        config.lifecycle.pre_verification = hooks;
        config
            .validate()
            .map_err(|error| LegacySettingsImportError::Unsupported(error.to_string()))?;
        sqlx::query("UPDATE projects SET environment_config=$1, image_hash=NULL WHERE id=$2")
            .bind(serde_json::to_value(config).expect("EnvironmentConfig serializes"))
            .bind(project_id)
            .execute(&mut *tx)
            .await?;
        record(
            &mut tx,
            project_id,
            "succeeded",
            "imported into projects.environment_config",
        )
        .await?;
        tx.commit().await?;
            Ok(LegacySettingsImportResult::Imported)
        }
        .await;

        match outcome {
            Ok(result) => Ok(result),
            Err(error) => {
                self.record_failure(project_id, &error.to_string()).await?;
                Err(error)
            }
        }
    }

    async fn record_failure(
        &self,
        project_id: &str,
        detail: &str,
    ) -> Result<(), LegacySettingsImportError> {
        let mut tx = self.db.pool().begin().await?;
        record(&mut tx, project_id, "failed", detail).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn failed_project_ids(&self) -> Result<Vec<String>, LegacySettingsImportError> {
        self.db.ensure_initialized().await?;
        Ok(sqlx::query_scalar("SELECT project_id FROM project_live_state_migrations WHERE family=$1 AND release=$2 AND result='failed'")
            .bind(FAMILY).bind(RELEASE).fetch_all(self.db.pool()).await?)
    }
}

async fn record(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    project_id: &str,
    result: &str,
    detail: &str,
) -> Result<(), LegacySettingsImportError> {
    sqlx::query("INSERT INTO project_live_state_migrations (project_id,family,release,source_inventory,destination,result,detail,rollback_instruction,finalized_at) VALUES ($1,$2,$3,'{\"path\":\".djinn/settings.json\"}'::jsonb,'projects.environment_config',$4,$5,'one-time direct import',to_char(now() at time zone 'utc','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')) ON CONFLICT (project_id,family,release) DO UPDATE SET result=EXCLUDED.result,detail=EXCLUDED.detail,updated_at=to_char(now() at time zone 'utc','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'),finalized_at=EXCLUDED.finalized_at")
        .bind(project_id)
        .bind(FAMILY)
        .bind(RELEASE)
        .bind(result)
        .bind(detail)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::seed_project;

    async fn fresh_importer(project_id: &str) -> LegacySettingsImport {
        let db = Database::ephemeral()
            .await
            .expect("open isolated Postgres database");
        seed_project(&db, project_id, project_id).await;
        LegacySettingsImport::new(db)
    }

    async fn config(importer: &LegacySettingsImport, project_id: &str) -> Value {
        sqlx::query_scalar("SELECT environment_config FROM projects WHERE id=$1")
            .bind(project_id)
            .fetch_one(importer.db.pool())
            .await
            .expect("read environment configuration")
    }

    #[tokio::test]
    async fn public_importer_maps_empty_and_populated_settings_losslessly() {
        let empty = fresh_importer("legacy-empty").await;
        assert_eq!(
            empty.import("legacy-empty", br#"{}"#).await.unwrap(),
            LegacySettingsImportResult::Imported
        );
        let empty_config: EnvironmentConfig =
            serde_json::from_value(config(&empty, "legacy-empty").await).unwrap();
        assert!(empty_config.agent_mcp_defaults.is_empty());
        assert!(empty_config.lifecycle.pre_verification.is_empty());

        let populated = fresh_importer("legacy-populated").await;
        let source = br#"{"agent_mcp_defaults":{"worker":["github","linear"]},"setup":["make setup","./bootstrap"]}"#;
        assert_eq!(
            populated.import("legacy-populated", source).await.unwrap(),
            LegacySettingsImportResult::Imported
        );
        let saved: EnvironmentConfig =
            serde_json::from_value(config(&populated, "legacy-populated").await).unwrap();
        assert_eq!(saved.agent_mcp_defaults["worker"], ["github", "linear"]);
        assert_eq!(
            saved.lifecycle.pre_verification,
            vec![
                HookCommand::Shell("make setup".into()),
                HookCommand::Shell("./bootstrap".into())
            ]
        );
    }

    #[tokio::test]
    async fn rejected_input_preserves_config_and_records_actionable_durable_failure() {
        let importer = fresh_importer("legacy-failure").await;
        let before = config(&importer, "legacy-failure").await;
        for source in [
            br#"{"global_skills":["retired"]}"#.as_slice(),
            br#"{"unknown":true}"#.as_slice(),
        ] {
            assert!(importer.import("legacy-failure", source).await.is_err());
            assert_eq!(config(&importer, "legacy-failure").await, before);
        }
        let outcome: (String, String) = sqlx::query_as("SELECT result, detail FROM project_live_state_migrations WHERE project_id=$1 AND family=$2 AND release=$3")
            .bind("legacy-failure").bind(FAMILY).bind(RELEASE).fetch_one(importer.db.pool()).await.expect("read durable failure");
        assert_eq!(outcome.0, "failed");
        assert!(outcome.1.contains("unparseable input"));
        let reconstructed = LegacySettingsImport::new(importer.db.clone());
        assert_eq!(
            reconstructed.failed_project_ids().await.unwrap(),
            vec!["legacy-failure"]
        );
    }

    #[tokio::test]
    async fn conflict_and_transaction_failure_never_partially_update_configuration() {
        let importer = fresh_importer("legacy-atomic").await;
        sqlx::query("UPDATE projects SET environment_config=$1 WHERE id=$2")
            .bind(serde_json::json!({"agent_mcp_defaults":{"worker":["existing"]},"lifecycle":{"pre_verification":[]}}))
            .bind("legacy-atomic").execute(importer.db.pool()).await.unwrap();
        let before = config(&importer, "legacy-atomic").await;
        assert!(
            importer
                .import(
                    "legacy-atomic",
                    br#"{"agent_mcp_defaults":{"worker":["different"]}}"#
                )
                .await
                .is_err()
        );
        assert_eq!(config(&importer, "legacy-atomic").await, before);

        let failing = fresh_importer("legacy-rollback").await;
        let rollback_before = config(&failing, "legacy-rollback").await;
        sqlx::query("ALTER TABLE project_live_state_migrations ADD CONSTRAINT reject_legacy_success_for_test CHECK (result <> 'succeeded')")
            .execute(failing.db.pool()).await.unwrap();
        assert!(
            failing
                .import("legacy-rollback", br#"{"setup":["make setup"]}"#)
                .await
                .is_err()
        );
        assert_eq!(config(&failing, "legacy-rollback").await, rollback_before);
        assert_eq!(
            failing.failed_project_ids().await.unwrap(),
            vec!["legacy-rollback"]
        );
    }
}
