//! Attempt-scoped immutable direct-delivery ledger. This module intentionally
//! owns every direct writer; it does not construct Git candidates or touch refs.

use std::str::FromStr;

use djinn_core::models::{
    ReworkDelivery, TaskDelivery, TaskDeliveryIdentity, TaskDeliveryState, TaskIntegrated,
};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};

use super::*;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryPrepareInput {
    pub identity: TaskDeliveryIdentity,
    pub transition_id: String,
    pub source_sha: String,
    pub patch_digest: String,
    pub selected_parent_sha: String,
    pub candidate_sha: String,
}
fn validate_task_integration(input: &TaskIntegrated) -> Result<()> {
    input.identity.validate()?;
    nonblank("candidate_sha", &input.candidate_sha)?;
    nonblank(
        "observed_applied_candidate_sha",
        &input.observed_applied_candidate_sha,
    )?;
    nonblank("merge_commit_sha", &input.merge_commit_sha)?;
    if input.candidate_sha != input.observed_applied_candidate_sha
        || input.candidate_sha != input.merge_commit_sha
    {
        return Err(Error::InvalidTransition(
            "task integration requires the exact observed delivery candidate".into(),
        ));
    }
    Ok(())
}

fn same_rework_preparation(
    row: &TaskDelivery,
    input: &DeliveryReworkInput,
    identity: &TaskDeliveryIdentity,
) -> bool {
    input.rework.delivery_generation == identity.delivery_generation
        && row.identity == *identity
        && row.prepare_transition_id == input.rework.transition_id
        && row.source_sha == input.source_sha
        && row.patch_digest == input.patch_digest
        && row.selected_parent_sha == input.selected_parent_sha
        && row.candidate_sha == input.candidate_sha
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryFinalizeInput {
    pub identity: TaskDeliveryIdentity,
    pub transition_id: String,
    pub conflict_reason: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeliveryReworkInput {
    pub rework: ReworkDelivery,
    pub source_sha: String,
    pub patch_digest: String,
    pub selected_parent_sha: String,
    pub candidate_sha: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeliveryTransitionResult {
    Applied(TaskDelivery),
    Replayed(TaskDelivery),
    Stale { current: Option<TaskDelivery> },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum TaskIntegrationResult {
    Integrated(Task),
    Replayed(Task),
    Stale {
        delivery: Option<TaskDelivery>,
        task_status: Option<String>,
    },
}

impl TaskRepository {
    /// Read the immutable ledger row by its complete delivery identity.
    pub async fn get_delivery(
        &self,
        identity: &TaskDeliveryIdentity,
    ) -> Result<Option<TaskDelivery>> {
        identity.validate()?;
        sqlx::query_as::<_, DeliveryRow>(&format!(
            "SELECT {COLS} FROM task_deliveries WHERE build_attempt_id=$1 AND task_id=$2 AND delivery_generation=$3"
        ))
        .bind(&identity.build_attempt_id)
        .bind(&identity.task_id)
        .bind(identity.delivery_generation)
        .fetch_optional(self.db.pool())
        .await?
        .map(DeliveryRow::into_delivery)
        .transpose()
    }

    pub async fn prepare_delivery(
        &self,
        input: &DeliveryPrepareInput,
    ) -> Result<DeliveryTransitionResult> {
        validate_prepare(input)?;
        self.require_direct_delivery_active().await?;
        let mut tx = self.db.pool().begin().await?;
        lock_attempt_and_task(&mut tx, &input.identity).await?;
        let existing = delivery_tx(&mut tx, &input.identity).await?;
        let result = match existing {
            Some(row) if same_preparation(&row, input) => DeliveryTransitionResult::Replayed(row),
            Some(row) => DeliveryTransitionResult::Stale { current: Some(row) },
            None => {
                let row = sqlx::query_as::<_, DeliveryRow>(&format!("INSERT INTO task_deliveries (build_attempt_id, task_id, delivery_generation, state, candidate_sha, base_sha, source_sha, patch_digest, selected_parent_sha, prepare_transition_id) VALUES ($1,$2,$3,'prepared',$4,$5,$6,$7,$8,$9) RETURNING {COLS}" )).bind(&input.identity.build_attempt_id).bind(&input.identity.task_id).bind(input.identity.delivery_generation).bind(&input.candidate_sha).bind(&input.selected_parent_sha).bind(&input.source_sha).bind(&input.patch_digest).bind(&input.selected_parent_sha).bind(&input.transition_id).fetch_one(&mut *tx).await?;
                DeliveryTransitionResult::Applied(row.into_delivery()?)
            }
        };
        tx.commit().await?;
        Ok(result)
    }

    pub async fn begin_delivery_apply(
        &self,
        input: &DeliveryFinalizeInput,
    ) -> Result<DeliveryTransitionResult> {
        self.transition_delivery(input, TaskDeliveryState::Applying)
            .await
    }
    pub async fn finalize_delivery_applied(
        &self,
        input: &DeliveryFinalizeInput,
    ) -> Result<DeliveryTransitionResult> {
        self.transition_delivery(input, TaskDeliveryState::Applied)
            .await
    }
    pub async fn finalize_delivery_conflict(
        &self,
        input: &DeliveryFinalizeInput,
    ) -> Result<DeliveryTransitionResult> {
        if input
            .conflict_reason
            .as_deref()
            .is_none_or(|v| v.trim().is_empty())
        {
            return Err(Error::InvalidTransition(
                "delivery conflict requires a reason".into(),
            ));
        }
        self.transition_delivery(input, TaskDeliveryState::Conflict)
            .await
    }

    async fn transition_delivery(
        &self,
        input: &DeliveryFinalizeInput,
        target: TaskDeliveryState,
    ) -> Result<DeliveryTransitionResult> {
        input.identity.validate()?;
        nonblank("transition_id", &input.transition_id)?;
        self.require_direct_delivery_active().await?;
        let mut tx = self.db.pool().begin().await?;
        lock_attempt_and_task(&mut tx, &input.identity).await?;
        let Some(current) = delivery_tx(&mut tx, &input.identity).await? else {
            return Ok(DeliveryTransitionResult::Stale { current: None });
        };
        if current.state == target
            && final_transition(&mut tx, &input.identity).await?.as_deref()
                == Some(&input.transition_id)
        {
            tx.commit().await?;
            return Ok(DeliveryTransitionResult::Replayed(current));
        }
        let legal = matches!(
            (current.state, target),
            (TaskDeliveryState::Prepared, TaskDeliveryState::Applying)
                | (TaskDeliveryState::Applying, TaskDeliveryState::Applied)
                | (TaskDeliveryState::Applying, TaskDeliveryState::Conflict)
        );
        if !legal {
            tx.commit().await?;
            return Ok(DeliveryTransitionResult::Stale {
                current: Some(current),
            });
        }
        let row = match target {
            TaskDeliveryState::Applying => sqlx::query_as::<_, DeliveryRow>(&format!("UPDATE task_deliveries SET state='applying', applying_transition_id=$1 WHERE build_attempt_id=$2 AND task_id=$3 AND delivery_generation=$4 AND state='prepared' RETURNING {COLS}")).bind(&input.transition_id).bind(&input.identity.build_attempt_id).bind(&input.identity.task_id).bind(input.identity.delivery_generation).fetch_optional(&mut *tx).await?,
            TaskDeliveryState::Applied => sqlx::query_as::<_, DeliveryRow>(&format!("UPDATE task_deliveries SET state='applied', applied_at=now(), finalization_transition_id=$1 WHERE build_attempt_id=$2 AND task_id=$3 AND delivery_generation=$4 AND state='applying' RETURNING {COLS}")).bind(&input.transition_id).bind(&input.identity.build_attempt_id).bind(&input.identity.task_id).bind(input.identity.delivery_generation).fetch_optional(&mut *tx).await?,
            TaskDeliveryState::Conflict => sqlx::query_as::<_, DeliveryRow>(&format!("UPDATE task_deliveries SET state='conflict', conflict_reason=$1, finalization_transition_id=$2 WHERE build_attempt_id=$3 AND task_id=$4 AND delivery_generation=$5 AND state='applying' RETURNING {COLS}")).bind(input.conflict_reason.as_deref()).bind(&input.transition_id).bind(&input.identity.build_attempt_id).bind(&input.identity.task_id).bind(input.identity.delivery_generation).fetch_optional(&mut *tx).await?,
            TaskDeliveryState::Prepared => None,
        };
        let result = match row {
            Some(row) => DeliveryTransitionResult::Applied(row.into_delivery()?),
            None => DeliveryTransitionResult::Stale {
                current: delivery_tx(&mut tx, &input.identity).await?,
            },
        };
        tx.commit().await?;
        Ok(result)
    }

    pub async fn rework_delivery(
        &self,
        input: &DeliveryReworkInput,
    ) -> Result<DeliveryTransitionResult> {
        nonblank("transition_id", &input.rework.transition_id)?;
        nonblank("build_attempt_id", &input.rework.build_attempt_id)?;
        nonblank("task_id", &input.rework.task_id)?;
        nonblank("source_sha", &input.source_sha)?;
        nonblank("patch_digest", &input.patch_digest)?;
        nonblank("selected_parent_sha", &input.selected_parent_sha)?;
        nonblank("candidate_sha", &input.candidate_sha)?;
        self.require_direct_delivery_active().await?;
        let old_id = TaskDeliveryIdentity::new(
            &input.rework.build_attempt_id,
            &input.rework.task_id,
            input.rework.expected_generation,
        )?;
        let new_id = TaskDeliveryIdentity::new(
            &input.rework.build_attempt_id,
            &input.rework.task_id,
            input.rework.delivery_generation,
        )?;
        if new_id.delivery_generation != old_id.delivery_generation + 1 {
            return Err(Error::InvalidTransition(
                "rework delivery_generation must be exactly expected_generation + 1".into(),
            ));
        }
        let mut tx = self.db.pool().begin().await?;
        lock_attempt_and_task(&mut tx, &old_id).await?;
        if let Some(row) =
            delivery_by_prepare_transition_tx(&mut tx, &old_id, &input.rework.transition_id).await?
        {
            if !same_rework_preparation(&row, input, &new_id) {
                return Err(Error::InvalidTransition(
                    "reused rework transition_id has different immutable command facts".into(),
                ));
            }
            tx.commit().await?;
            return Ok(DeliveryTransitionResult::Replayed(row));
        }
        let Some(old) = delivery_tx(&mut tx, &old_id).await? else {
            return Ok(DeliveryTransitionResult::Stale { current: None });
        };
        let latest: Option<i64> = sqlx::query_scalar("SELECT max(delivery_generation) FROM task_deliveries WHERE build_attempt_id=$1 AND task_id=$2").bind(&old_id.build_attempt_id).bind(&old_id.task_id).fetch_one(&mut *tx).await?;
        if old.state != TaskDeliveryState::Conflict
            || latest != Some(old_id.delivery_generation)
            || old.source_sha == input.source_sha
        {
            tx.commit().await?;
            return Ok(DeliveryTransitionResult::Stale { current: Some(old) });
        }
        let row = sqlx::query_as::<_, DeliveryRow>(&format!("INSERT INTO task_deliveries (build_attempt_id,task_id,delivery_generation,state,candidate_sha,base_sha,source_sha,patch_digest,selected_parent_sha,prepare_transition_id) VALUES ($1,$2,$3,'prepared',$4,$5,$6,$7,$8,$9) RETURNING {COLS}")).bind(&new_id.build_attempt_id).bind(&new_id.task_id).bind(new_id.delivery_generation).bind(&input.candidate_sha).bind(&input.selected_parent_sha).bind(&input.source_sha).bind(&input.patch_digest).bind(&input.selected_parent_sha).bind(&input.rework.transition_id).fetch_one(&mut *tx).await?;
        let row = row.into_delivery()?;
        tx.commit().await?;
        Ok(DeliveryTransitionResult::Applied(row))
    }

    pub async fn task_integrated(&self, input: &TaskIntegrated) -> Result<TaskIntegrationResult> {
        validate_task_integration(input)?;
        self.require_direct_delivery_active().await?;
        let mut tx = self.db.pool().begin().await?;
        lock_attempt_and_task(&mut tx, &input.identity).await?;
        let delivery = delivery_tx(&mut tx, &input.identity).await?;
        let task: Task = task_select_where_id!(&input.identity.task_id)
            .fetch_one(&mut *tx)
            .await?;
        if task.status == "closed"
            && task.merge_commit_sha.as_deref() == Some(&input.merge_commit_sha)
            && delivery.as_ref().is_some_and(|row| {
                row.state == TaskDeliveryState::Applied
                    && row.candidate_sha == input.candidate_sha
                    && input.candidate_sha == input.observed_applied_candidate_sha
                    && input.candidate_sha == input.merge_commit_sha
            })
        {
            tx.commit().await?;
            return Ok(TaskIntegrationResult::Replayed(task));
        }
        let Some(delivery) = delivery else {
            tx.commit().await?;
            return Ok(TaskIntegrationResult::Stale {
                delivery: None,
                task_status: Some(task.status),
            });
        };
        if task.status != "approved"
            || delivery.state != TaskDeliveryState::Applying
            || delivery.candidate_sha != input.candidate_sha
            || input.candidate_sha != input.observed_applied_candidate_sha
            || input.candidate_sha != input.merge_commit_sha
        {
            tx.commit().await?;
            return Ok(TaskIntegrationResult::Stale {
                delivery: Some(delivery),
                task_status: Some(task.status),
            });
        }
        let head_update = sqlx::query("UPDATE proposal_build_attempts SET branch_head_sha=$1 WHERE id=$2 AND branch_head_sha IS NOT DISTINCT FROM $3")
            .bind(&input.candidate_sha)
            .bind(&input.identity.build_attempt_id)
            .bind(&delivery.selected_parent_sha)
            .execute(&mut *tx)
            .await?;
        if head_update.rows_affected() != 1 {
            return Ok(TaskIntegrationResult::Stale {
                delivery: Some(delivery),
                task_status: Some(task.status),
            });
        }
        let delivery_update = sqlx::query("UPDATE task_deliveries SET state='applied', applied_at=now(), finalization_transition_id=COALESCE(finalization_transition_id, 'task_integrated') WHERE build_attempt_id=$1 AND task_id=$2 AND delivery_generation=$3 AND state='applying'").bind(&input.identity.build_attempt_id).bind(&input.identity.task_id).bind(input.identity.delivery_generation).execute(&mut *tx).await?;
        if delivery_update.rows_affected() != 1 {
            return Ok(TaskIntegrationResult::Stale {
                delivery: Some(delivery),
                task_status: Some(task.status),
            });
        }
        let close_update = sqlx::query("UPDATE tasks SET status='closed', merge_commit_sha=$1, close_reason='completed', closed_at=to_char(now() at time zone 'utc','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"'), updated_at=to_char(now() at time zone 'utc','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') WHERE id=$2 AND status='approved'").bind(&input.merge_commit_sha).bind(&input.identity.task_id).execute(&mut *tx).await?;
        if close_update.rows_affected() != 1 {
            return Ok(TaskIntegrationResult::Stale {
                delivery: Some(delivery),
                task_status: Some(task.status),
            });
        }
        sqlx::query("INSERT INTO activity_log (id,task_id,actor_id,actor_role,event_type,payload) VALUES ($1,$2,'system','system','status_changed',$3::jsonb)").bind(uuid::Uuid::now_v7().to_string()).bind(&input.identity.task_id).bind(serde_json::json!({"from_status":"approved","to_status":"closed","reason":"direct_delivery_integrated"}).to_string()).execute(&mut *tx).await?;
        let task: Task = task_select_where_id!(&input.identity.task_id)
            .fetch_one(&mut *tx)
            .await?;
        // Preserve normal terminal status effects in this same transaction so
        // a late failure rolls back the ledger, attempt head, and task closure.
        super::adjudication_close::apply_adjudication_child_close_tx(
            &mut tx,
            &task.id,
            &task.labels,
        )
        .await?;
        crate::repositories::note::working_spec::archive_task_working_specs_tx(
            &mut tx,
            &task.id,
            "archived task working spec on terminal task state",
        )
        .await?;
        tx.commit().await?;
        self.events
            .send(DjinnEventEnvelope::task_updated(&task, false));
        self.emit_unblocked_tasks(&task.id).await?;
        Ok(TaskIntegrationResult::Integrated(task))
    }

    async fn require_direct_delivery_active(&self) -> Result<()> {
        use crate::repositories::direct_delivery_capability::{
            DirectDeliveryCapabilityRepository, DirectDeliverySchemaCapability,
        };
        match DirectDeliveryCapabilityRepository::new(self.db.clone())
            .probe()
            .await?
        {
            DirectDeliverySchemaCapability::SupportedActive { .. } => Ok(()),
            _ => Err(Error::InvalidTransition(
                "direct_delivery_v1 capability is unavailable or disabled".into(),
            )),
        }
    }
}

#[derive(sqlx::FromRow)]
struct DeliveryRow {
    build_attempt_id: String,
    task_id: String,
    delivery_generation: i64,
    state: String,
    candidate_sha: String,
    base_sha: String,
    source_sha: String,
    patch_digest: String,
    selected_parent_sha: String,
    prepare_transition_id: String,
    applied_at: Option<String>,
    conflict_reason: Option<String>,
    created_at: String,
}
impl DeliveryRow {
    fn into_delivery(self) -> Result<TaskDelivery> {
        Ok(TaskDelivery {
            identity: TaskDeliveryIdentity::new(
                self.build_attempt_id,
                self.task_id,
                self.delivery_generation,
            )?,
            state: TaskDeliveryState::from_str(&self.state).map_err(Error::InvalidData)?,
            candidate_sha: self.candidate_sha,
            source_sha: self.source_sha,
            patch_digest: self.patch_digest,
            selected_parent_sha: self.selected_parent_sha,
            prepare_transition_id: self.prepare_transition_id,
            base_sha: self.base_sha,
            applied_at: self.applied_at,
            conflict_reason: self.conflict_reason,
            created_at: self.created_at,
        })
    }
}
const COLS: &str = "build_attempt_id,task_id,delivery_generation,state,candidate_sha,base_sha,source_sha,patch_digest,selected_parent_sha,prepare_transition_id,to_char(applied_at AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS applied_at,conflict_reason,to_char(created_at AT TIME ZONE 'UTC','YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS created_at";
async fn lock_attempt_and_task(
    tx: &mut Transaction<'_, Postgres>,
    id: &TaskDeliveryIdentity,
) -> Result<()> {
    sqlx::query("SELECT id FROM proposal_build_attempts WHERE id=$1 FOR UPDATE")
        .bind(&id.build_attempt_id)
        .execute(&mut **tx)
        .await?;
    sqlx::query("SELECT id FROM tasks WHERE id=$1 FOR UPDATE")
        .bind(&id.task_id)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
async fn delivery_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: &TaskDeliveryIdentity,
) -> Result<Option<TaskDelivery>> {
    sqlx::query_as::<_,DeliveryRow>(&format!("SELECT {COLS} FROM task_deliveries WHERE build_attempt_id=$1 AND task_id=$2 AND delivery_generation=$3 FOR UPDATE")).bind(&id.build_attempt_id).bind(&id.task_id).bind(id.delivery_generation).fetch_optional(&mut **tx).await?.map(DeliveryRow::into_delivery).transpose()
}
async fn final_transition(
    tx: &mut Transaction<'_, Postgres>,
    id: &TaskDeliveryIdentity,
) -> Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT COALESCE(finalization_transition_id,applying_transition_id) FROM task_deliveries WHERE build_attempt_id=$1 AND task_id=$2 AND delivery_generation=$3").bind(&id.build_attempt_id).bind(&id.task_id).bind(id.delivery_generation).fetch_one(&mut **tx).await?)
}
async fn delivery_by_prepare_transition_tx(
    tx: &mut Transaction<'_, Postgres>,
    id: &TaskDeliveryIdentity,
    transition: &str,
) -> Result<Option<TaskDelivery>> {
    sqlx::query_as::<_,DeliveryRow>(&format!("SELECT {COLS} FROM task_deliveries WHERE build_attempt_id=$1 AND task_id=$2 AND prepare_transition_id=$3 FOR UPDATE")).bind(&id.build_attempt_id).bind(&id.task_id).bind(transition).fetch_optional(&mut **tx).await?.map(DeliveryRow::into_delivery).transpose()
}
fn nonblank(n: &str, v: &str) -> Result<()> {
    if v.trim().is_empty() {
        Err(Error::InvalidTransition(format!("{n} must be nonblank")))
    } else {
        Ok(())
    }
}
fn validate_prepare(i: &DeliveryPrepareInput) -> Result<()> {
    i.identity.validate()?;
    nonblank("transition_id", &i.transition_id)?;
    nonblank("source_sha", &i.source_sha)?;
    nonblank("patch_digest", &i.patch_digest)?;
    nonblank("selected_parent_sha", &i.selected_parent_sha)?;
    nonblank("candidate_sha", &i.candidate_sha)
}
fn same_preparation(a: &TaskDelivery, b: &DeliveryPrepareInput) -> bool {
    a.prepare_transition_id == b.transition_id
        && a.source_sha == b.source_sha
        && a.patch_digest == b.patch_digest
        && a.selected_parent_sha == b.selected_parent_sha
        && a.candidate_sha == b.candidate_sha
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rework_input(generation: i64) -> DeliveryReworkInput {
        DeliveryReworkInput {
            rework: ReworkDelivery {
                transition_id: "rework-transition".into(),
                build_attempt_id: "attempt".into(),
                task_id: "task".into(),
                expected_generation: 1,
                delivery_generation: generation,
            },
            source_sha: "new-source".into(),
            patch_digest: "patch".into(),
            selected_parent_sha: "parent".into(),
            candidate_sha: "candidate".into(),
        }
    }

    fn reworked_row() -> TaskDelivery {
        TaskDelivery {
            identity: TaskDeliveryIdentity::new("attempt", "task", 2).unwrap(),
            state: TaskDeliveryState::Prepared,
            candidate_sha: "candidate".into(),
            source_sha: "new-source".into(),
            patch_digest: "patch".into(),
            selected_parent_sha: "parent".into(),
            prepare_transition_id: "rework-transition".into(),
            base_sha: "parent".into(),
            applied_at: None,
            conflict_reason: None,
            created_at: "now".into(),
        }
    }

    #[test]
    fn rework_replay_requires_exact_generation_and_immutable_facts() {
        let identity = TaskDeliveryIdentity::new("attempt", "task", 2).unwrap();
        let row = reworked_row();
        assert!(same_rework_preparation(&row, &rework_input(2), &identity));
        assert!(!same_rework_preparation(&row, &rework_input(3), &identity));

        let mut different_candidate = rework_input(2);
        different_candidate.candidate_sha = "other-candidate".into();
        assert!(!same_rework_preparation(
            &row,
            &different_candidate,
            &identity
        ));
    }
}
