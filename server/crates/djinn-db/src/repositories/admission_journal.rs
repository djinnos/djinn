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

/// An Observe-mode reservation and its reference-cap decision.
///
/// The reservation is always admitted, but `would_defer` is calculated while
/// holding the same capacity lock as the insert so telemetry cannot miss a
/// concurrent admission.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObserveAdmissionResult {
    pub reservation: ReserveAdmissionResult,
    pub would_defer: bool,
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdoptLiveAdmissionInput {
    pub key: AdmissionJournalKey,
    pub workload_kind: AdmissionWorkloadKind,
    pub creator_server_epoch: String,
    pub object_name: String,
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

    /// Atomically record an Observe-mode reservation and reference-cap result.
    ///
    /// Unlike [`Self::reserve`], this never denies a new task/warm row. The
    /// reference-cap decision and insert share the advisory-lock transaction.
    pub async fn reserve_observed(
        &self,
        input: &ReserveAdmissionInput,
        reference_cap: i64,
    ) -> DbResult<ObserveAdmissionResult> {
        if reference_cap < 0 {
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
            return Ok(ObserveAdmissionResult {
                reservation: ReserveAdmissionResult::Reserved {
                    row,
                    idempotent: true,
                },
                would_defer: false,
            });
        }

        let occupancy = count_occupancy_tx(&mut tx).await?;
        let would_defer = matches!(
            input.key.domain,
            AdmissionDomain::TaskObservation | AdmissionDomain::WarmBuild
        ) && occupancy >= reference_cap;
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

        Ok(ObserveAdmissionResult {
            reservation: ReserveAdmissionResult::Reserved {
                row: row.try_into()?,
                idempotent: false,
            },
            would_defer,
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

    pub async fn adopt_live(
        &self,
        input: &AdoptLiveAdmissionInput,
    ) -> DbResult<AdmissionJournalRow> {
        if input.object_uid.trim().is_empty() {
            return Err(DbError::InvalidData("inventory UID is empty".into()));
        }
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock_capacity(&mut tx).await?;
        if let Some(row) = fetch_row(&mut tx, &input.key).await? {
            if row.state == AdmissionState::Live
                && row.object_uid.as_deref() == Some(input.object_uid.as_str())
            {
                tx.commit().await?;
                return Ok(row);
            }
            if row.state == AdmissionState::CreateUnknown
                && row.object_name == input.object_name
                && row.object_uid.is_none()
            {
                let adopted =
                    update_state(&mut tx, &input.key, "live", Some(&input.object_uid)).await?;
                tx.commit().await?;
                return Ok(adopted);
            }
            return Err(DbError::InvalidTransition(
                "inventory identity collision".into(),
            ));
        }
        let row = sqlx::query_as::<_, JournalDbRow>("INSERT INTO admission_journal (domain, work_id, generation, workload_kind, state, creator_server_epoch, object_name, object_uid) VALUES ($1,$2,$3,$4,'live',$5,$6,$7) RETURNING domain, work_id, generation, workload_kind, state, creator_server_epoch, object_name, object_uid, created_at::text, updated_at::text, terminal_at::text").bind(input.key.domain.as_str()).bind(&input.key.work_id).bind(input.key.generation).bind(input.workload_kind.as_str()).bind(&input.creator_server_epoch).bind(&input.object_name).bind(&input.object_uid).fetch_one(&mut *tx).await?;
        tx.commit().await?;
        row.try_into()
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

    /// Atomically recover every predecessor epoch in a single transaction.
    ///
    /// On startup a replacement process does not know the exact predecessor
    /// epoch string(s). This primitive retires every Reserved row and converts
    /// every CreateInFlight row to occupying CreateUnknown for all rows whose
    /// `creator_server_epoch` differs from the current server epoch. It then
    /// returns all active rows so the controller can seed occupancy without
    /// duplicating permits.
    ///
    /// This extends [`Self::recover_predecessor_epoch`] with the all-predecessor
    /// recovery primitive required for cold restart; the single-epoch variant is
    /// retained for tests and targeted reconciliation.
    pub async fn recover_all_predecessors(
        &self,
        current_server_epoch: &str,
    ) -> DbResult<AdmissionRecoveryResult> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let retired_reserved = sqlx::query(
            "UPDATE admission_journal SET state = 'terminal', terminal_at = now(), updated_at = now() \
             WHERE creator_server_epoch <> $1 AND state = 'reserved'",
        )
        .bind(current_server_epoch)
        .execute(&mut *tx)
        .await?
        .rows_affected();
        let marked_create_unknown = sqlx::query(
            "UPDATE admission_journal SET state = 'create_unknown', updated_at = now() \
             WHERE creator_server_epoch <> $1 AND state = 'create_in_flight'",
        )
        .bind(current_server_epoch)
        .execute(&mut *tx)
        .await?
        .rows_affected();
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

    fn create_started(input: &ReserveAdmissionInput) -> CreateStartedInput {
        CreateStartedInput {
            key: input.key.clone(),
            creator_server_epoch: input.creator_server_epoch.clone(),
            object_name: input.object_name.clone(),
        }
    }

    fn uid_input(input: &ReserveAdmissionInput, object_uid: &str) -> UidFencedAdmissionInput {
        UidFencedAdmissionInput {
            key: input.key.clone(),
            object_uid: object_uid.into(),
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

    #[tokio::test]
    async fn definitive_and_ambiguous_create_failures_have_distinct_occupancy() {
        let db = Database::open_in_memory().unwrap();
        let repo = AdmissionJournalRepository::new(db);
        let reserved = input(AdmissionDomain::TaskObservation, "definitive-reserved", 0);
        let in_flight = input(AdmissionDomain::TaskObservation, "definitive-flight", 0);
        let ambiguous = input(AdmissionDomain::TaskObservation, "ambiguous", 0);
        for reservation in [&reserved, &in_flight, &ambiguous] {
            repo.reserve(reservation, 3).await.unwrap();
        }

        assert_eq!(
            repo.mark_definitive_create_failure(&reserved.key)
                .await
                .unwrap()
                .state,
            AdmissionState::Terminal
        );
        assert_eq!(
            repo.mark_definitive_create_failure(&reserved.key)
                .await
                .unwrap()
                .state,
            AdmissionState::Terminal
        );
        repo.mark_create_started(&create_started(&in_flight))
            .await
            .unwrap();
        assert_eq!(
            repo.mark_definitive_create_failure(&in_flight.key)
                .await
                .unwrap()
                .state,
            AdmissionState::Terminal
        );

        repo.mark_create_started(&create_started(&ambiguous))
            .await
            .unwrap();
        assert_eq!(
            repo.mark_create_started(&create_started(&ambiguous))
                .await
                .unwrap()
                .state,
            AdmissionState::CreateInFlight
        );
        assert_eq!(
            repo.mark_create_unknown(&ambiguous.key)
                .await
                .unwrap()
                .state,
            AdmissionState::CreateUnknown
        );
        assert_eq!(
            repo.mark_create_unknown(&ambiguous.key)
                .await
                .unwrap()
                .state,
            AdmissionState::CreateUnknown
        );
        // LIST absence is deliberately not an input to this repository: ambiguity occupies.
        assert_eq!(repo.count_task_or_warm_occupancy().await.unwrap(), 1);
        assert_eq!(repo.list_active_rows().await.unwrap()[0].key, ambiguous.key);
        assert_eq!(
            repo.mark_live(&uid_input(&ambiguous, "uid-ambiguous"))
                .await
                .unwrap()
                .state,
            AdmissionState::Live
        );
    }

    #[tokio::test]
    async fn cancellation_is_reserved_only_and_idempotent() {
        let db = Database::open_in_memory().unwrap();
        let repo = AdmissionJournalRepository::new(db);
        let cancelled = input(AdmissionDomain::TaskObservation, "cancelled", 0);
        let posted = input(AdmissionDomain::TaskObservation, "posted", 0);
        repo.reserve(&cancelled, 2).await.unwrap();
        repo.reserve(&posted, 2).await.unwrap();
        assert_eq!(
            repo.cancel_reserved(&cancelled.key).await.unwrap().state,
            AdmissionState::Terminal
        );
        assert_eq!(
            repo.cancel_reserved(&cancelled.key).await.unwrap().state,
            AdmissionState::Terminal
        );
        repo.mark_create_started(&create_started(&posted))
            .await
            .unwrap();
        assert!(matches!(
            repo.cancel_reserved(&posted.key).await,
            Err(DbError::InvalidTransition(_))
        ));
        assert_eq!(repo.count_task_or_warm_occupancy().await.unwrap(), 1);
    }

    #[tokio::test]
    async fn stale_generations_and_mismatched_uids_cannot_release_current_work() {
        let db = Database::open_in_memory().unwrap();
        let repo = AdmissionJournalRepository::new(db);
        let first = input(AdmissionDomain::TaskObservation, "fenced", 0);
        repo.reserve(&first, 1).await.unwrap();
        repo.mark_create_started(&create_started(&first))
            .await
            .unwrap();
        repo.mark_live(&uid_input(&first, "uid-first"))
            .await
            .unwrap();
        repo.mark_terminal(&TerminalAdmissionInput {
            key: first.key.clone(),
            object_uid: Some("uid-first".into()),
        })
        .await
        .unwrap();

        let second = input(AdmissionDomain::TaskObservation, "fenced", 1);
        repo.reserve(&second, 1).await.unwrap();
        repo.mark_create_started(&create_started(&second))
            .await
            .unwrap();
        repo.mark_live(&uid_input(&second, "uid-current"))
            .await
            .unwrap();
        assert!(matches!(
            repo.mark_live(&uid_input(&first, "uid-first")).await,
            Err(DbError::InvalidTransition(_))
        ));
        assert!(matches!(
            repo.mark_terminal(&TerminalAdmissionInput {
                key: second.key.clone(),
                object_uid: Some("wrong-uid".into()),
            })
            .await,
            Err(DbError::InvalidTransition(_))
        ));
        assert_eq!(
            repo.mark_live(&uid_input(&second, "uid-current"))
                .await
                .unwrap()
                .state,
            AdmissionState::Live
        );
        assert_eq!(repo.count_task_or_warm_occupancy().await.unwrap(), 1);
        for _ in 0..2 {
            assert_eq!(
                repo.mark_terminal(&TerminalAdmissionInput {
                    key: second.key.clone(),
                    object_uid: Some("uid-current".into()),
                })
                .await
                .unwrap()
                .state,
                AdmissionState::Terminal
            );
        }
        assert_eq!(repo.count_task_or_warm_occupancy().await.unwrap(), 0);
    }

    #[tokio::test]
    async fn predecessor_recovery_retires_only_reserved_and_retains_ambiguous_work() {
        let db = Database::open_in_memory().unwrap();
        let repo = AdmissionJournalRepository::new(db);
        let reserved = input(AdmissionDomain::TaskObservation, "recover-reserved", 0);
        let flight = input(AdmissionDomain::TaskObservation, "recover-flight", 0);
        let unknown = input(AdmissionDomain::TaskObservation, "recover-unknown", 0);
        let live = input(AdmissionDomain::TaskObservation, "recover-live", 0);
        let mut successor = input(AdmissionDomain::TaskObservation, "recover-successor", 0);
        successor.creator_server_epoch = "epoch-2".into();
        for reservation in [&reserved, &flight, &unknown, &live, &successor] {
            repo.reserve(reservation, 5).await.unwrap();
        }
        repo.mark_create_started(&create_started(&flight))
            .await
            .unwrap();
        repo.mark_create_started(&create_started(&unknown))
            .await
            .unwrap();
        repo.mark_create_unknown(&unknown.key).await.unwrap();
        repo.mark_create_started(&create_started(&live))
            .await
            .unwrap();
        repo.mark_live(&uid_input(&live, "uid-live")).await.unwrap();

        let report = repo.recover_predecessor_epoch("epoch-1").await.unwrap();
        assert_eq!(report.retired_reserved, 1);
        assert_eq!(report.marked_create_unknown, 1);
        let states = report
            .active_rows
            .iter()
            .map(|row| (row.key.work_id.as_str(), row.state))
            .collect::<Vec<_>>();
        assert_eq!(
            states,
            vec![
                ("recover-flight", AdmissionState::CreateUnknown),
                ("recover-live", AdmissionState::Live),
                ("recover-successor", AdmissionState::Reserved),
                ("recover-unknown", AdmissionState::CreateUnknown),
            ]
        );
    }

    #[tokio::test]
    async fn recover_all_predecessors_retires_every_predecessor_epoch_atomically() {
        let db = Database::open_in_memory().unwrap();
        let repo = AdmissionJournalRepository::new(db);
        // Two distinct predecessor epochs plus the current replacement epoch.
        let mut pred_a = input(AdmissionDomain::WarmBuild, "pred-a-reserved", 0);
        pred_a.creator_server_epoch = "epoch-a".into();
        let mut pred_a_flight = input(AdmissionDomain::WarmBuild, "pred-a-flight", 0);
        pred_a_flight.creator_server_epoch = "epoch-a".into();
        let mut pred_b = input(AdmissionDomain::WarmBuild, "pred-b-reserved", 0);
        pred_b.creator_server_epoch = "epoch-b".into();
        let mut current = input(AdmissionDomain::WarmBuild, "current-reserved", 0);
        current.creator_server_epoch = "replacement-epoch".into();
        for reservation in [&pred_a, &pred_a_flight, &pred_b, &current] {
            repo.reserve(reservation, 10).await.unwrap();
        }
        repo.mark_create_started(&create_started(&pred_a_flight))
            .await
            .unwrap();

        // recover_all_predecessors processes every epoch except the current one.
        let report = repo
            .recover_all_predecessors("replacement-epoch")
            .await
            .unwrap();
        assert_eq!(
            report.retired_reserved, 2,
            "both predecessor Reserved retired"
        );
        assert_eq!(
            report.marked_create_unknown, 1,
            "the single predecessor CreateInFlight converted to CreateUnknown"
        );
        // The current-epoch Reserved row is untouched.
        let states = report
            .active_rows
            .iter()
            .map(|row| (row.key.work_id.as_str(), row.state))
            .collect::<Vec<_>>();
        assert_eq!(
            states,
            vec![
                ("current-reserved", AdmissionState::Reserved),
                ("pred-a-flight", AdmissionState::CreateUnknown),
            ]
        );
    }
}
