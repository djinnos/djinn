//! Durable admission journal reservation primitives.
//!
//! This repository owns storage invariants and atomic capacity accounting only.
//! Admission policy, workload classification, and lifecycle orchestration remain
//! in higher layers.

use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};

use crate::database::Database;
use crate::error::{DbError, DbResult};

const OCCUPYING_STATES: [&str; 4] = ["reserved", "create_in_flight", "create_unknown", "live"];

/// Namespace for an admission journal work identity.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionDomain {
    TaskObservation,
    WarmBuild,
    /// Reserved for the emergency-to-invocation handoff protocol.
    InvocationBuild,
}

impl AdmissionDomain {
    fn as_str(self) -> &'static str {
        match self {
            Self::TaskObservation => "task_observation",
            Self::WarmBuild => "warm_build",
            Self::InvocationBuild => "invocation_build",
        }
    }

    fn parse(value: &str) -> DbResult<Self> {
        match value {
            "task_observation" => Ok(Self::TaskObservation),
            "warm_build" => Ok(Self::WarmBuild),
            "invocation_build" => Ok(Self::InvocationBuild),
            _ => Err(DbError::InvalidData(format!(
                "invalid admission domain `{value}`"
            ))),
        }
    }
}

/// Kind of workload represented by a journal row.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionWorkloadKind {
    Task,
    Warm,
    Invocation,
}

impl AdmissionWorkloadKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Task => "task",
            Self::Warm => "warm",
            Self::Invocation => "invocation",
        }
    }

    fn parse(value: &str) -> DbResult<Self> {
        match value {
            "task" => Ok(Self::Task),
            "warm" => Ok(Self::Warm),
            "invocation" => Ok(Self::Invocation),
            _ => Err(DbError::InvalidData(format!(
                "invalid admission workload kind `{value}`"
            ))),
        }
    }
}

/// Durable state of an admission generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AdmissionState {
    Reserved,
    CreateInFlight,
    CreateUnknown,
    Live,
    /// Retained audit state; terminal rows no longer consume capacity.
    Terminal,
}

impl AdmissionState {
    fn parse(value: &str) -> DbResult<Self> {
        match value {
            "reserved" => Ok(Self::Reserved),
            "create_in_flight" => Ok(Self::CreateInFlight),
            "create_unknown" => Ok(Self::CreateUnknown),
            "live" => Ok(Self::Live),
            "terminal" => Ok(Self::Terminal),
            _ => Err(DbError::InvalidData(format!(
                "invalid admission state `{value}`"
            ))),
        }
    }
}

/// Unique identity for one work generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionJournalKey {
    pub domain: AdmissionDomain,
    pub work_id: String,
    pub generation: i64,
}

/// Input required to reserve one journal generation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReserveAdmissionInput {
    pub key: AdmissionJournalKey,
    pub workload_kind: AdmissionWorkloadKind,
    pub creator_server_epoch: String,
    pub object_name: String,
}

/// Durable journal record. Timestamps are RFC3339-formatted by PostgreSQL.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionJournalRow {
    pub key: AdmissionJournalKey,
    pub workload_kind: AdmissionWorkloadKind,
    pub state: AdmissionState,
    pub creator_server_epoch: String,
    pub object_name: String,
    pub object_uid: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub terminal_at: Option<String>,
}

/// Result of a capacity reservation attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReserveAdmissionResult {
    /// A new row was atomically reserved, or the exact journal key already existed.
    Reserved {
        row: AdmissionJournalRow,
        idempotent: bool,
    },
    /// The selected task/warm occupancy was already at capacity.
    Denied { occupancy: i64, cap: i64 },
}

/// Identity verified and durably recorded before a Kubernetes POST.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateStartedInput {
    pub key: AdmissionJournalKey,
    pub creator_server_epoch: String,
    pub object_name: String,
}

/// Kubernetes callback fenced by the observed object UID.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UidFencedAdmissionInput {
    pub key: AdmissionJournalKey,
    pub object_uid: String,
}

/// Terminal mutation input; a UID is required for a Live row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalAdmissionInput {
    pub key: AdmissionJournalKey,
    pub object_uid: Option<String>,
}

/// Atomic predecessor-epoch recovery report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionRecoveryResult {
    pub retired_reserved: u64,
    pub marked_create_unknown: u64,
    pub active_rows: Vec<AdmissionJournalRow>,
}

/// Atomic Postgres repository for admission reservations and journal history.
pub struct AdmissionJournalRepository {
    db: Database,
}

impl AdmissionJournalRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Atomically reserve a generation under a task/warm occupancy cap.
    ///
    /// A transaction-scoped advisory lock serializes the count-and-insert for a
    /// shared cap across both task-observation and warm-build domains. Existing
    /// keys are returned before capacity denial, making retries idempotent and
    /// preventing a duplicate request from consuming capacity twice.
    pub async fn reserve(
        &self,
        input: &ReserveAdmissionInput,
        cap: i64,
    ) -> DbResult<ReserveAdmissionResult> {
        if cap < 0 {
            return Err(DbError::InvalidData(
                "admission cap must be non-negative".into(),
            ));
        }
        if input.key.generation < 0 {
            return Err(DbError::InvalidData(
                "admission generation must be non-negative".into(),
            ));
        }
        self.db.ensure_initialized().await?;

        let mut tx = self.db.pool().begin().await?;
        lock_capacity(&mut tx).await?;

        if let Some(row) = fetch_row(&mut tx, &input.key).await? {
            tx.commit().await?;
            return Ok(ReserveAdmissionResult::Reserved {
                row,
                idempotent: true,
            });
        }

        let occupancy = count_occupancy_tx(&mut tx).await?;
        if matches!(
            input.key.domain,
            AdmissionDomain::TaskObservation | AdmissionDomain::WarmBuild
        ) && occupancy >= cap
        {
            tx.commit().await?;
            return Ok(ReserveAdmissionResult::Denied { occupancy, cap });
        }

        let row = sqlx::query_as::<_, JournalDbRow>(
            "INSERT INTO admission_journal \
             (domain, work_id, generation, workload_kind, state, creator_server_epoch, object_name) \
             VALUES ($1, $2, $3, $4, 'reserved', $5, $6) \
             RETURNING domain, work_id, generation, workload_kind, state, creator_server_epoch, \
                       object_name, object_uid, created_at::text, updated_at::text, terminal_at::text",
        )
        .bind(input.key.domain.as_str())
        .bind(&input.key.work_id)
        .bind(input.key.generation)
        .bind(input.workload_kind.as_str())
        .bind(&input.creator_server_epoch)
        .bind(&input.object_name)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;

        Ok(ReserveAdmissionResult::Reserved {
            row: row.try_into()?,
            idempotent: false,
        })
    }

    pub async fn mark_create_started(
        &self,
        input: &CreateStartedInput,
    ) -> DbResult<AdmissionJournalRow> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let row = current_row_for_update(&mut tx, &input.key).await?;
        if row.creator_server_epoch != input.creator_server_epoch
            || row.object_name != input.object_name
        {
            return Err(DbError::InvalidTransition(
                "create identity differs from reservation".into(),
            ));
        }
        let result = match row.state {
            AdmissionState::Reserved => {
                update_state(&mut tx, &input.key, "create_in_flight", None).await?
            }
            AdmissionState::CreateInFlight => row,
            state => return Err(invalid_state("mark create started", state)),
        };
        tx.commit().await?;
        Ok(result)
    }

    pub async fn mark_create_unknown(
        &self,
        key: &AdmissionJournalKey,
    ) -> DbResult<AdmissionJournalRow> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let row = current_row_for_update(&mut tx, key).await?;
        let result = match row.state {
            AdmissionState::CreateInFlight => {
                update_state(&mut tx, key, "create_unknown", None).await?
            }
            AdmissionState::CreateUnknown => row,
            state => return Err(invalid_state("mark create unknown", state)),
        };
        tx.commit().await?;
        Ok(result)
    }

    pub async fn mark_live(
        &self,
        input: &UidFencedAdmissionInput,
    ) -> DbResult<AdmissionJournalRow> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let row = current_row_for_update(&mut tx, &input.key).await?;
        if row
            .object_uid
            .as_deref()
            .is_some_and(|uid| uid != input.object_uid)
        {
            return Err(DbError::InvalidTransition(
                "Kubernetes UID does not match admission row".into(),
            ));
        }
        let result = match row.state {
            AdmissionState::CreateInFlight | AdmissionState::CreateUnknown => {
                update_state(&mut tx, &input.key, "live", Some(&input.object_uid)).await?
            }
            AdmissionState::Live => row,
            state => return Err(invalid_state("mark live", state)),
        };
        tx.commit().await?;
        Ok(result)
    }

    pub async fn mark_definitive_create_failure(
        &self,
        key: &AdmissionJournalKey,
    ) -> DbResult<AdmissionJournalRow> {
        self.mark_terminal_from_states(
            key,
            &[AdmissionState::Reserved, AdmissionState::CreateInFlight],
            "mark definitive create failure",
        )
        .await
    }

    pub async fn cancel_reserved(
        &self,
        key: &AdmissionJournalKey,
    ) -> DbResult<AdmissionJournalRow> {
        self.mark_terminal_from_states(key, &[AdmissionState::Reserved], "cancel reserved")
            .await
    }

    pub async fn mark_terminal(
        &self,
        input: &TerminalAdmissionInput,
    ) -> DbResult<AdmissionJournalRow> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let row = current_row_for_update(&mut tx, &input.key).await?;
        let result = match row.state {
            AdmissionState::Live if row.object_uid.as_deref() == input.object_uid.as_deref() => {
                update_state(&mut tx, &input.key, "terminal", row.object_uid.as_deref()).await?
            }
            AdmissionState::Terminal
                if row.object_uid.as_deref() == input.object_uid.as_deref() =>
            {
                row
            }
            AdmissionState::Live | AdmissionState::Terminal => {
                return Err(DbError::InvalidTransition(
                    "Kubernetes UID does not match admission row".into(),
                ));
            }
            state => return Err(invalid_state("mark terminal", state)),
        };
        tx.commit().await?;
        Ok(result)
    }

    async fn mark_terminal_from_states(
        &self,
        key: &AdmissionJournalKey,
        allowed: &[AdmissionState],
        operation: &str,
    ) -> DbResult<AdmissionJournalRow> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let row = current_row_for_update(&mut tx, key).await?;
        let result = if allowed.contains(&row.state) {
            update_state(&mut tx, key, "terminal", None).await?
        } else if row.state == AdmissionState::Terminal {
            row
        } else {
            return Err(invalid_state(operation, row.state));
        };
        tx.commit().await?;
        Ok(result)
    }

    pub async fn list_active_rows(&self) -> DbResult<Vec<AdmissionJournalRow>> {
        self.db.ensure_initialized().await?;
        active_rows(self.db.pool()).await
    }

    pub async fn recover_predecessor_epoch(
        &self,
        predecessor_epoch: &str,
    ) -> DbResult<AdmissionRecoveryResult> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let retired_reserved = sqlx::query("UPDATE admission_journal SET state = 'terminal', terminal_at = now(), updated_at = now() WHERE creator_server_epoch = $1 AND state = 'reserved'").bind(predecessor_epoch).execute(&mut *tx).await?.rows_affected();
        let marked_create_unknown = sqlx::query("UPDATE admission_journal SET state = 'create_unknown', updated_at = now() WHERE creator_server_epoch = $1 AND state = 'create_in_flight'").bind(predecessor_epoch).execute(&mut *tx).await?.rows_affected();
        let rows = active_rows(&mut *tx).await?;
        tx.commit().await?;
        Ok(AdmissionRecoveryResult {
            retired_reserved,
            marked_create_unknown,
            active_rows: rows,
        })
    }

    /// Count rows that currently occupy task-or-warm capacity.
    pub async fn count_task_or_warm_occupancy(&self) -> DbResult<i64> {
        self.db.ensure_initialized().await?;
        sqlx::query_scalar(
            "SELECT COUNT(*)::bigint FROM admission_journal \
             WHERE domain IN ('task_observation', 'warm_build') \
               AND state IN ('reserved', 'create_in_flight', 'create_unknown', 'live')",
        )
        .fetch_one(self.db.pool())
        .await
        .map_err(Into::into)
    }

    /// Return retained history for one work item, including terminal generations.
    pub async fn list_history(
        &self,
        domain: AdmissionDomain,
        work_id: &str,
    ) -> DbResult<Vec<AdmissionJournalRow>> {
        self.db.ensure_initialized().await?;
        let rows = sqlx::query_as::<_, JournalDbRow>(
            "SELECT domain, work_id, generation, workload_kind, state, creator_server_epoch, \
                    object_name, object_uid, created_at::text, updated_at::text, terminal_at::text \
             FROM admission_journal WHERE domain = $1 AND work_id = $2 ORDER BY generation ASC",
        )
        .bind(domain.as_str())
        .bind(work_id)
        .fetch_all(self.db.pool())
        .await?;
        rows.into_iter().map(TryInto::try_into).collect()
    }

    /// Allocate the next generation only if the latest retained generation is terminal.
    ///
    /// Allocation intentionally does not insert a row: callers must reserve the
    /// returned generation through [`Self::reserve`].
    pub async fn allocate_next_generation(
        &self,
        domain: AdmissionDomain,
        work_id: &str,
    ) -> DbResult<i64> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock_work(&mut tx, domain, work_id).await?;
        let latest: Option<(i64, String)> = sqlx::query_as(
            "SELECT generation, state FROM admission_journal \
             WHERE domain = $1 AND work_id = $2 ORDER BY generation DESC LIMIT 1",
        )
        .bind(domain.as_str())
        .bind(work_id)
        .fetch_optional(&mut *tx)
        .await?;

        let next = match latest {
            None => 0,
            Some((generation, state)) if state == "terminal" => generation + 1,
            Some((generation, state)) => {
                return Err(DbError::InvalidTransition(format!(
                    "cannot allocate generation after nonterminal admission generation {generation} ({state})"
                )));
            }
        };
        tx.commit().await?;
        Ok(next)
    }
}

#[derive(sqlx::FromRow)]
struct JournalDbRow {
    domain: String,
    work_id: String,
    generation: i64,
    workload_kind: String,
    state: String,
    creator_server_epoch: String,
    object_name: String,
    object_uid: Option<String>,
    created_at: String,
    updated_at: String,
    terminal_at: Option<String>,
}

impl TryFrom<JournalDbRow> for AdmissionJournalRow {
    type Error = DbError;

    fn try_from(value: JournalDbRow) -> Result<Self, Self::Error> {
        Ok(Self {
            key: AdmissionJournalKey {
                domain: AdmissionDomain::parse(&value.domain)?,
                work_id: value.work_id,
                generation: value.generation,
            },
            workload_kind: AdmissionWorkloadKind::parse(&value.workload_kind)?,
            state: AdmissionState::parse(&value.state)?,
            creator_server_epoch: value.creator_server_epoch,
            object_name: value.object_name,
            object_uid: value.object_uid,
            created_at: value.created_at,
            updated_at: value.updated_at,
            terminal_at: value.terminal_at,
        })
    }
}

async fn lock_capacity(tx: &mut Transaction<'_, Postgres>) -> DbResult<()> {
    sqlx::query(
        "SELECT pg_advisory_xact_lock(hashtextextended('admission-task-warm-capacity', 0))",
    )
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn lock_work(
    tx: &mut Transaction<'_, Postgres>,
    domain: AdmissionDomain,
    work_id: &str,
) -> DbResult<()> {
    let lock_key = format!("admission-generation:{}:{work_id}", domain.as_str());
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(lock_key)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn count_occupancy_tx(tx: &mut Transaction<'_, Postgres>) -> DbResult<i64> {
    sqlx::query_scalar(
        "SELECT COUNT(*)::bigint FROM admission_journal \
         WHERE domain IN ('task_observation', 'warm_build') \
           AND state = ANY($1)",
    )
    .bind(OCCUPYING_STATES.as_slice())
    .fetch_one(&mut **tx)
    .await
    .map_err(Into::into)
}

async fn fetch_row(
    tx: &mut Transaction<'_, Postgres>,
    key: &AdmissionJournalKey,
) -> DbResult<Option<AdmissionJournalRow>> {
    let row = sqlx::query_as::<_, JournalDbRow>(
        "SELECT domain, work_id, generation, workload_kind, state, creator_server_epoch, \
                object_name, object_uid, created_at::text, updated_at::text, terminal_at::text \
         FROM admission_journal WHERE domain = $1 AND work_id = $2 AND generation = $3",
    )
    .bind(key.domain.as_str())
    .bind(&key.work_id)
    .bind(key.generation)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(TryInto::try_into).transpose()
}

const JOURNAL_COLUMNS: &str = "domain, work_id, generation, workload_kind, state, creator_server_epoch, object_name, object_uid, created_at::text, updated_at::text, terminal_at::text";

fn invalid_state(operation: &str, state: AdmissionState) -> DbError {
    DbError::InvalidTransition(format!("cannot {operation} from {state:?}"))
}

async fn current_row_for_update(
    tx: &mut Transaction<'_, Postgres>,
    key: &AdmissionJournalKey,
) -> DbResult<AdmissionJournalRow> {
    lock_work(tx, key.domain, &key.work_id).await?;
    let latest: Option<i64> = sqlx::query_scalar("SELECT generation FROM admission_journal WHERE domain = $1 AND work_id = $2 ORDER BY generation DESC LIMIT 1")
        .bind(key.domain.as_str()).bind(&key.work_id).fetch_optional(&mut **tx).await?;
    if latest != Some(key.generation) {
        return Err(DbError::InvalidTransition(format!(
            "stale admission generation {} for {}",
            key.generation, key.work_id
        )));
    }
    let row = sqlx::query_as::<_, JournalDbRow>(&format!("SELECT {JOURNAL_COLUMNS} FROM admission_journal WHERE domain = $1 AND work_id = $2 AND generation = $3 FOR UPDATE"))
        .bind(key.domain.as_str()).bind(&key.work_id).bind(key.generation).fetch_one(&mut **tx).await?;
    row.try_into()
}

async fn update_state(
    tx: &mut Transaction<'_, Postgres>,
    key: &AdmissionJournalKey,
    state: &str,
    object_uid: Option<&str>,
) -> DbResult<AdmissionJournalRow> {
    let row = sqlx::query_as::<_, JournalDbRow>(&format!("UPDATE admission_journal SET state = $1, object_uid = COALESCE($2, object_uid), updated_at = now(), terminal_at = CASE WHEN $3 THEN now() ELSE terminal_at END WHERE domain = $4 AND work_id = $5 AND generation = $6 RETURNING {JOURNAL_COLUMNS}"))
        .bind(state).bind(object_uid).bind(state == "terminal").bind(key.domain.as_str()).bind(&key.work_id).bind(key.generation).fetch_one(&mut **tx).await?;
    row.try_into()
}

async fn active_rows<'e, E>(executor: E) -> DbResult<Vec<AdmissionJournalRow>>
where
    E: sqlx::Executor<'e, Database = Postgres>,
{
    let rows = sqlx::query_as::<_, JournalDbRow>(&format!("SELECT {JOURNAL_COLUMNS} FROM admission_journal WHERE state = ANY($1) ORDER BY domain, work_id, generation"))
        .bind(OCCUPYING_STATES.as_slice()).fetch_all(executor).await?;
    rows.into_iter().map(TryInto::try_into).collect()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn input(domain: AdmissionDomain, work_id: &str, generation: i64) -> ReserveAdmissionInput {
        ReserveAdmissionInput {
            key: AdmissionJournalKey {
                domain,
                work_id: work_id.into(),
                generation,
            },
            workload_kind: match domain {
                AdmissionDomain::TaskObservation => AdmissionWorkloadKind::Task,
                AdmissionDomain::WarmBuild => AdmissionWorkloadKind::Warm,
                AdmissionDomain::InvocationBuild => AdmissionWorkloadKind::Invocation,
            },
            creator_server_epoch: "epoch-1".into(),
            object_name: format!("admission-{work_id}-{generation}"),
        }
    }

    async fn set_state(db: &Database, key: &AdmissionJournalKey, state: &str) {
        sqlx::query(
            "UPDATE admission_journal SET state = $1, terminal_at = \
             CASE WHEN $1 = 'terminal' THEN now() ELSE NULL END WHERE domain = $2 AND work_id = $3 AND generation = $4",
        )
        .bind(state)
        .bind(key.domain.as_str())
        .bind(&key.work_id)
        .bind(key.generation)
        .execute(db.pool())
        .await
        .unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cap_one_concurrent_reservation_has_one_winner() {
        let db = Database::open_in_memory().unwrap();
        let repo = Arc::new(AdmissionJournalRepository::new(db));
        let first = {
            let repo = Arc::clone(&repo);
            tokio::spawn(async move {
                repo.reserve(&input(AdmissionDomain::TaskObservation, "a", 0), 1)
                    .await
                    .unwrap()
            })
        };
        let second = {
            let repo = Arc::clone(&repo);
            tokio::spawn(async move {
                repo.reserve(&input(AdmissionDomain::WarmBuild, "b", 0), 1)
                    .await
                    .unwrap()
            })
        };
        let results = [first.await.unwrap(), second.await.unwrap()];
        assert_eq!(
            results
                .iter()
                .filter(|r| matches!(r, ReserveAdmissionResult::Reserved { .. }))
                .count(),
            1
        );
        assert_eq!(repo.count_task_or_warm_occupancy().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn duplicate_reservation_is_idempotent() {
        let db = Database::open_in_memory().unwrap();
        let repo = AdmissionJournalRepository::new(db);
        let input = input(AdmissionDomain::TaskObservation, "same", 0);
        assert!(matches!(
            repo.reserve(&input, 1).await.unwrap(),
            ReserveAdmissionResult::Reserved {
                idempotent: false,
                ..
            }
        ));
        assert!(matches!(
            repo.reserve(&input, 1).await.unwrap(),
            ReserveAdmissionResult::Reserved {
                idempotent: true,
                ..
            }
        ));
        assert_eq!(repo.count_task_or_warm_occupancy().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn all_occupying_states_count_but_terminal_history_does_not() {
        let db = Database::open_in_memory().unwrap();
        let repo = AdmissionJournalRepository::new(db.clone());
        for (index, state) in [
            "reserved",
            "create_in_flight",
            "create_unknown",
            "live",
            "terminal",
        ]
        .iter()
        .enumerate()
        {
            let input = input(
                AdmissionDomain::TaskObservation,
                &format!("work-{index}"),
                0,
            );
            repo.reserve(&input, 10).await.unwrap();
            set_state(&db, &input.key, state).await;
        }
        assert_eq!(repo.count_task_or_warm_occupancy().await.unwrap(), 4);
        let history = repo
            .list_history(AdmissionDomain::TaskObservation, "work-4")
            .await
            .unwrap();
        assert_eq!(history[0].state, AdmissionState::Terminal);
    }

    #[tokio::test]
    async fn reservation_domains_are_separate_but_task_warm_share_cap() {
        let db = Database::open_in_memory().unwrap();
        let repo = AdmissionJournalRepository::new(db);
        assert!(matches!(
            repo.reserve(&input(AdmissionDomain::InvocationBuild, "same", 0), 0)
                .await
                .unwrap(),
            ReserveAdmissionResult::Reserved { .. }
        ));
        assert!(matches!(
            repo.reserve(&input(AdmissionDomain::TaskObservation, "same", 0), 1)
                .await
                .unwrap(),
            ReserveAdmissionResult::Reserved { .. }
        ));
        assert!(matches!(
            repo.reserve(&input(AdmissionDomain::WarmBuild, "same", 0), 1)
                .await
                .unwrap(),
            ReserveAdmissionResult::Denied { .. }
        ));
    }

    #[tokio::test]
    async fn next_generation_requires_terminal_predecessor() {
        let db = Database::open_in_memory().unwrap();
        let repo = AdmissionJournalRepository::new(db.clone());
        let input = input(AdmissionDomain::TaskObservation, "history", 0);
        assert_eq!(
            repo.allocate_next_generation(AdmissionDomain::TaskObservation, "history")
                .await
                .unwrap(),
            0
        );
        repo.reserve(&input, 1).await.unwrap();
        assert!(matches!(
            repo.allocate_next_generation(AdmissionDomain::TaskObservation, "history")
                .await,
            Err(DbError::InvalidTransition(_))
        ));
        set_state(&db, &input.key, "terminal").await;
        assert_eq!(
            repo.allocate_next_generation(AdmissionDomain::TaskObservation, "history")
                .await
                .unwrap(),
            1
        );
    }
}
