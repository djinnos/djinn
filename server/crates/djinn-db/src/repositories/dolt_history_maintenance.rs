use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::{Database, DatabaseBackendKind, Error, MysqlBackendFlavor};

const MAIN_BRANCH: &str = "main";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoltHistoryMaintenancePolicy {
    pub compact_commit_threshold: u64,
    pub flatten_commit_threshold: u64,
    pub flatten_hour_utc: u8,
    pub execution_enabled: bool,
}

impl Default for DoltHistoryMaintenancePolicy {
    fn default() -> Self {
        Self {
            compact_commit_threshold: 2_000,
            flatten_commit_threshold: 5_000,
            flatten_hour_utc: 3,
            execution_enabled: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoltHistoryMaintenanceAction {
    None,
    Compact,
    Flatten,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoltHistoryTableCount {
    pub table: String,
    pub row_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoltHistoryMaintenanceSnapshot {
    pub commit_count: u64,
    pub current_hour_utc: u8,
    pub non_main_branches: Vec<String>,
    pub row_counts: Vec<DoltHistoryTableCount>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoltHistoryMaintenancePlan {
    pub action: DoltHistoryMaintenanceAction,
    pub reason: String,
    pub commit_count: u64,
    pub current_hour_utc: u8,
    pub non_main_branches: Vec<String>,
    pub baseline_row_counts: Vec<DoltHistoryTableCount>,
    pub safety_warnings: Vec<String>,
    pub verification_required: bool,
    pub execution_enabled: bool,
}

impl DoltHistoryMaintenancePlan {
    pub fn is_safe_to_execute(&self) -> bool {
        self.action != DoltHistoryMaintenanceAction::None && self.safety_warnings.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DoltHistoryMaintenanceExecution {
    UnsupportedBackend,
    NoActionRequired,
    BlockedBySafetyChecks,
    PlannedOnly,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DoltHistoryMaintenanceReport {
    pub plan: DoltHistoryMaintenancePlan,
    pub execution: DoltHistoryMaintenanceExecution,
}

#[derive(Debug, thiserror::Error)]
pub enum DoltHistoryMaintenanceError {
    #[error("database backend `{backend}` does not support Dolt history maintenance")]
    UnsupportedBackend { backend: String },
    #[error("dolt history maintenance sql failed during {action}: {message}")]
    Sql {
        action: &'static str,
        message: String,
    },
    #[error("unsafe table name returned from metadata query: {0}")]
    UnsafeTableName(String),
    #[error("row-count verification mismatch: {details}")]
    VerificationMismatch { details: String },
    #[error(transparent)]
    Db(#[from] Error),
}

pub struct DoltHistoryMaintenanceService<'a> {
    db: &'a Database,
}

impl<'a> DoltHistoryMaintenanceService<'a> {
    pub fn new(db: &'a Database) -> Self {
        Self { db }
    }

    pub fn is_dolt_backend(&self) -> bool {
        let capabilities = self.db.backend_capabilities();
        capabilities.backend_kind == DatabaseBackendKind::Mysql
            && capabilities.supports_branching_metadata
            && capabilities.backend_label == MysqlBackendFlavor::Dolt.as_str()
    }

    pub async fn plan_current_maintenance(
        &self,
        policy: &DoltHistoryMaintenancePolicy,
        current_hour_utc: u8,
    ) -> Result<DoltHistoryMaintenancePlan, DoltHistoryMaintenanceError> {
        self.ensure_dolt_backend()?;
        let snapshot = self.snapshot(current_hour_utc).await?;
        Ok(plan_dolt_history_maintenance(policy, &snapshot))
    }

    pub async fn scheduled_report(
        &self,
        policy: &DoltHistoryMaintenancePolicy,
        current_hour_utc: u8,
    ) -> Result<DoltHistoryMaintenanceReport, DoltHistoryMaintenanceError> {
        let plan = self
            .plan_current_maintenance(policy, current_hour_utc)
            .await?;
        let execution = if plan.action == DoltHistoryMaintenanceAction::None {
            DoltHistoryMaintenanceExecution::NoActionRequired
        } else if !plan.is_safe_to_execute() {
            DoltHistoryMaintenanceExecution::BlockedBySafetyChecks
        } else {
            // Explicit seam for ADR-055 lifecycle maintenance: coordinator now plans
            // and schedules this work, but destructive history rewriting stays gated
            // behind an explicit cutover instead of running automatically by default.
            let _ = policy.execution_enabled;
            DoltHistoryMaintenanceExecution::PlannedOnly
        };
        Ok(DoltHistoryMaintenanceReport { plan, execution })
    }

    async fn snapshot(
        &self,
        current_hour_utc: u8,
    ) -> Result<DoltHistoryMaintenanceSnapshot, DoltHistoryMaintenanceError> {
        self.ensure_dolt_backend()?;
        let Some(pool) = self.db.mysql_pool() else {
            return Err(DoltHistoryMaintenanceError::UnsupportedBackend {
                backend: self.db.bootstrap_info().backend_label.clone(),
            });
        };

        let commit_row = sqlx::query("SELECT COUNT(*) AS count FROM dolt_log")
            .fetch_one(pool)
            .await
            .map_err(|err| DoltHistoryMaintenanceError::Sql {
                action: "count_commits",
                message: err.to_string(),
            })?;
        let commit_count = decode_count(&commit_row, "count", "count_commits")?;

        let branch_rows = sqlx::query(
            "SELECT name FROM dolt_branches WHERE name NOT LIKE 'refs/%' ORDER BY name",
        )
        .fetch_all(pool)
        .await
        .map_err(|err| DoltHistoryMaintenanceError::Sql {
            action: "list_branches",
            message: err.to_string(),
        })?;
        let non_main_branches = branch_rows
            .into_iter()
            .filter_map(|row| row.try_get::<String, _>("name").ok())
            .filter(|name| name != MAIN_BRANCH)
            .collect::<Vec<_>>();

        let table_rows = sqlx::query(
            "SELECT table_name \
             FROM information_schema.tables \
             WHERE table_schema = DATABASE() \
               AND table_type = 'BASE TABLE' \
               AND table_name NOT LIKE 'dolt\\_%' ESCAPE '\\\\' \
             ORDER BY table_name",
        )
        .fetch_all(pool)
        .await
        .map_err(|err| DoltHistoryMaintenanceError::Sql {
            action: "list_application_tables",
            message: err.to_string(),
        })?;

        let mut row_counts = Vec::with_capacity(table_rows.len());
        for row in table_rows {
            let table = row.try_get::<String, _>("table_name").map_err(|err| {
                DoltHistoryMaintenanceError::Sql {
                    action: "list_application_tables",
                    message: err.to_string(),
                }
            })?;
            if !is_safe_identifier(&table) {
                return Err(DoltHistoryMaintenanceError::UnsafeTableName(table));
            }
            let sql = format!("SELECT COUNT(*) AS count FROM `{table}`");
            let count_row = sqlx::query(&sql).fetch_one(pool).await.map_err(|err| {
                DoltHistoryMaintenanceError::Sql {
                    action: "count_table_rows",
                    message: format!("{table}: {err}"),
                }
            })?;
            row_counts.push(DoltHistoryTableCount {
                table,
                row_count: decode_count(&count_row, "count", "count_table_rows")?,
            });
        }

        Ok(DoltHistoryMaintenanceSnapshot {
            commit_count,
            current_hour_utc,
            non_main_branches,
            row_counts,
        })
    }

    fn ensure_dolt_backend(&self) -> Result<(), DoltHistoryMaintenanceError> {
        if self.is_dolt_backend() {
            Ok(())
        } else {
            Err(DoltHistoryMaintenanceError::UnsupportedBackend {
                backend: self.db.backend_capabilities().backend_label.clone(),
            })
        }
    }
}

pub fn plan_dolt_history_maintenance(
    policy: &DoltHistoryMaintenancePolicy,
    snapshot: &DoltHistoryMaintenanceSnapshot,
) -> DoltHistoryMaintenancePlan {
    let mut safety_warnings = Vec::new();
    if !snapshot.non_main_branches.is_empty() {
        safety_warnings.push(format!(
            "refusing history maintenance while non-main branches exist: {}",
            snapshot.non_main_branches.join(", ")
        ));
    }
    if snapshot.row_counts.is_empty() {
        safety_warnings.push(
            "refusing history maintenance because baseline row-count verification is unavailable"
                .to_string(),
        );
    }

    let (action, reason) = if snapshot.commit_count >= policy.flatten_commit_threshold
        && snapshot.current_hour_utc == policy.flatten_hour_utc
    {
        (
            DoltHistoryMaintenanceAction::Flatten,
            format!(
                "flatten scheduled at {:02}:00 UTC with {} commits (threshold {})",
                snapshot.current_hour_utc, snapshot.commit_count, policy.flatten_commit_threshold
            ),
        )
    } else if snapshot.commit_count >= policy.compact_commit_threshold {
        (
            DoltHistoryMaintenanceAction::Compact,
            format!(
                "compact scheduled with {} commits (threshold {})",
                snapshot.commit_count, policy.compact_commit_threshold
            ),
        )
    } else {
        (
            DoltHistoryMaintenanceAction::None,
            format!(
                "history maintenance skipped: {} commits below compact threshold {}",
                snapshot.commit_count, policy.compact_commit_threshold
            ),
        )
    };

    DoltHistoryMaintenancePlan {
        action,
        reason,
        commit_count: snapshot.commit_count,
        current_hour_utc: snapshot.current_hour_utc,
        non_main_branches: snapshot.non_main_branches.clone(),
        baseline_row_counts: snapshot.row_counts.clone(),
        safety_warnings,
        verification_required: true,
        execution_enabled: policy.execution_enabled,
    }
}

pub fn verify_row_counts(
    expected: &[DoltHistoryTableCount],
    observed: &[DoltHistoryTableCount],
) -> Result<(), DoltHistoryMaintenanceError> {
    let expected_map = expected
        .iter()
        .map(|entry| (entry.table.clone(), entry.row_count))
        .collect::<BTreeMap<_, _>>();
    let observed_map = observed
        .iter()
        .map(|entry| (entry.table.clone(), entry.row_count))
        .collect::<BTreeMap<_, _>>();

    if expected_map == observed_map {
        return Ok(());
    }

    let tables = expected_map
        .keys()
        .chain(observed_map.keys())
        .cloned()
        .collect::<BTreeSet<_>>();
    let mut diffs = Vec::new();
    for table in tables {
        let before = expected_map.get(&table).copied();
        let after = observed_map.get(&table).copied();
        if before != after {
            diffs.push(format!("{table}: before={before:?}, after={after:?}"));
        }
    }

    Err(DoltHistoryMaintenanceError::VerificationMismatch {
        details: diffs.join("; "),
    })
}

fn decode_count(
    row: &sqlx::mysql::MySqlRow,
    column: &'static str,
    action: &'static str,
) -> Result<u64, DoltHistoryMaintenanceError> {
    let raw: i64 = row
        .try_get(column)
        .map_err(|err| DoltHistoryMaintenanceError::Sql {
            action,
            message: err.to_string(),
        })?;
    Ok(raw.max(0) as u64)
}

fn is_safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> DoltHistoryMaintenancePolicy {
        DoltHistoryMaintenancePolicy {
            compact_commit_threshold: 2_000,
            flatten_commit_threshold: 5_000,
            flatten_hour_utc: 3,
            execution_enabled: false,
        }
    }

    fn snapshot(commit_count: u64, current_hour_utc: u8) -> DoltHistoryMaintenanceSnapshot {
        DoltHistoryMaintenanceSnapshot {
            commit_count,
            current_hour_utc,
            non_main_branches: Vec::new(),
            row_counts: vec![DoltHistoryTableCount {
                table: "notes".to_string(),
                row_count: 42,
            }],
        }
    }

    #[test]
    fn prefers_flatten_at_scheduled_hour_once_threshold_is_met() {
        let plan = plan_dolt_history_maintenance(&policy(), &snapshot(5_500, 3));
        assert_eq!(plan.action, DoltHistoryMaintenanceAction::Flatten);
        assert!(plan.reason.contains("flatten scheduled"));
        assert!(plan.is_safe_to_execute());
    }

    #[test]
    fn falls_back_to_compact_outside_flatten_window() {
        let plan = plan_dolt_history_maintenance(&policy(), &snapshot(2_500, 1));
        assert_eq!(plan.action, DoltHistoryMaintenanceAction::Compact);
        assert!(plan.reason.contains("compact scheduled"));
    }

    #[test]
    fn blocks_destructive_maintenance_when_task_branches_are_present() {
        let mut snapshot = snapshot(5_500, 3);
        snapshot.non_main_branches = vec!["task/qhnb".to_string(), "task/he6k".to_string()];
        let plan = plan_dolt_history_maintenance(&policy(), &snapshot);
        assert_eq!(plan.action, DoltHistoryMaintenanceAction::Flatten);
        assert!(!plan.is_safe_to_execute());
        assert_eq!(plan.safety_warnings.len(), 1);
        assert!(plan.safety_warnings[0].contains("task/qhnb"));
    }

    #[test]
    fn row_count_verification_detects_destructive_drift() {
        let expected = vec![
            DoltHistoryTableCount {
                table: "notes".to_string(),
                row_count: 10,
            },
            DoltHistoryTableCount {
                table: "tasks".to_string(),
                row_count: 3,
            },
        ];
        let observed = vec![
            DoltHistoryTableCount {
                table: "notes".to_string(),
                row_count: 9,
            },
            DoltHistoryTableCount {
                table: "tasks".to_string(),
                row_count: 3,
            },
        ];

        let error = verify_row_counts(&expected, &observed).unwrap_err();
        assert!(matches!(
            error,
            DoltHistoryMaintenanceError::VerificationMismatch { .. }
        ));
        assert!(error.to_string().contains("notes"));
    }
}
