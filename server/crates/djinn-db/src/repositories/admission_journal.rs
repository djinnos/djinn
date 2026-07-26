//! Durable admission journal reservation primitives.
//!
//! This repository owns storage invariants and the durable OBJECT LIFECYCLE:
//! generation allocation, UID fencing, predecessor-epoch recovery, and
//! absent-object reclamation. It deliberately owns no capacity accounting --
//! see [`AdmissionJournalRepository::reserve`] for why that moved wholesale to
//! `BuildLeaseRepository`. Admission policy, workload classification, and
//! lifecycle orchestration remain in higher layers.

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

/// One recorded journal generation.
///
/// There is deliberately no `Denied` variant. This ledger has no cap and cannot
/// refuse; capacity is decided once, by `BuildLeaseRepository`, before a caller
/// ever reaches here. A denial-shaped result would re-open the door to a second
/// authority forming around it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReservedAdmission {
    pub row: AdmissionJournalRow,
    /// The exact journal key already existed and was returned unchanged.
    pub idempotent: bool,
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

/// One occupying generation and the exact identity an absence proof was taken
/// against.
///
/// Reclamation is a compare-and-set, never a blind write: every field below is
/// re-read under the row lock and must still match, so a row that changed after
/// the Kubernetes evidence was gathered — it acquired a UID, advanced to Live,
/// or was re-created by a newer dispatch — is refused rather than terminalized.
/// This is what keeps the existing fencing semantics intact while still
/// releasing capacity whose object is provably gone.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReclaimAbsentInput {
    pub key: AdmissionJournalKey,
    pub observed_state: AdmissionState,
    pub observed_creator_server_epoch: String,
    pub observed_object_name: String,
    pub observed_object_uid: Option<String>,
}

/// Outcome of one evidence-fenced reclamation attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReclaimAbsentOutcome {
    /// The row still matched the proof and was retired to Terminal.
    Reclaimed(AdmissionJournalRow),
    /// The row was already Terminal; capacity was released by someone else.
    AlreadyTerminal(AdmissionJournalRow),
    /// The row no longer matches the observation the proof was taken against;
    /// nothing was written.
    Fenced { reason: String },
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

    /// Record a generation in the lifecycle ledger. This NEVER denies.
    ///
    /// # This repository is deliberately not a capacity authority
    ///
    /// It used to be: `reserve` took a cap, counted occupancy across the
    /// task-observation and warm-build domains, and denied over it. That was one
    /// of TWO authorities enforcing `DJINN_MAX_BUILD_TASKRUNS`, and because the
    /// v1 build lease governed graph warming while this journal governed
    /// task-runs, the two caps covered disjoint populations and admitted 2x the
    /// operator's intent. Capacity accounting now lives solely in
    /// `build_leases` (`BuildLeaseRepository`), which can count every population
    /// in one transaction because they are rows in one table.
    ///
    /// What remains here is what a journal is uniquely good at, and what the
    /// lease cannot do: durable generation lifecycle
    /// ([`Self::resolve_dispatch_generation`]), Kubernetes UID fencing, restart
    /// recovery of a predecessor epoch, and absent-object reclamation
    /// ([`Self::reclaim_absent_object`]). A row is therefore written for EVERY
    /// created object regardless of which authority granted its capacity --
    /// including leased warm Jobs, which previously wrote no row at all and were
    /// consequently invisible to reclamation.
    ///
    /// The advisory lock is retained: it still serializes the fetch-and-insert
    /// so a duplicate request cannot write two rows for one key.
    pub async fn reserve(&self, input: &ReserveAdmissionInput) -> DbResult<ReservedAdmission> {
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
            return Ok(ReservedAdmission {
                row,
                idempotent: true,
            });
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

        Ok(ReservedAdmission {
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

    /// Record an ambiguous create outcome for this generation.
    ///
    /// A create report can legitimately arrive after the generation already
    /// reached its terminal outcome — the dispatch side effect and the
    /// lifecycle observation are separate messages with no ordering guarantee.
    /// A late create report is therefore a defined idempotent no-op that
    /// retains the terminal row: ambiguity about a create can never resurrect
    /// occupancy that a terminal observation already released. Stale
    /// generations are still rejected by [`current_row_for_update`].
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
            AdmissionState::CreateUnknown | AdmissionState::Terminal => row,
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

    /// Occupying rows, each flagged with whether it has been untouched for at
    /// least `settle_seconds`.
    ///
    /// Settlement is evaluated against the database clock that wrote
    /// `updated_at`, so it needs no timestamp parsing and no agreement between
    /// process clocks. It is NOT by itself a reason to release anything: a
    /// Kubernetes create that a dead process POSTed can still be admitted by
    /// the API server for a short window after the process is gone, and this
    /// flag is what stops reconciliation from racing that window. Absence of
    /// the object remains the only proof.
    pub async fn list_active_rows_with_settlement(
        &self,
        settle_seconds: i64,
    ) -> DbResult<Vec<(AdmissionJournalRow, bool)>> {
        if settle_seconds < 0 {
            return Err(DbError::InvalidData(
                "settle window must be non-negative".into(),
            ));
        }
        self.db.ensure_initialized().await?;
        let rows = sqlx::query_as::<_, SettlementDbRow>(&format!(
            "SELECT {JOURNAL_COLUMNS}, (updated_at <= now() - make_interval(secs => $2)) AS settled \
             FROM admission_journal WHERE state = ANY($1) ORDER BY domain, work_id, generation"
        ))
        .bind(OCCUPYING_STATES.as_slice())
        .bind(settle_seconds as f64)
        .fetch_all(self.db.pool())
        .await?;
        rows.into_iter()
            .map(|row| {
                let settled = row.settled.unwrap_or(false);
                AdmissionJournalRow::try_from(row.row).map(|row| (row, settled))
            })
            .collect()
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

    /// Retire one occupying generation whose Kubernetes object is provably gone.
    ///
    /// Recovery only terminalizes `reserved` rows and converts `create_in_flight`
    /// into occupying `create_unknown`. Nothing else in the journal can leave an
    /// occupying state without a lifecycle callback from the process that
    /// created the object, so a generation whose object vanished with its
    /// creator occupies capacity forever. This is the durable half of the
    /// reconciliation that fixes that; the caller owns the Kubernetes evidence.
    ///
    /// The write is fenced by a compare-and-set on the full observed identity
    /// (state, creator epoch, object name, object UID). Anything that changed
    /// since the proof was gathered yields [`ReclaimAbsentOutcome::Fenced`] and
    /// writes nothing, so this can never terminalize a row that might still
    /// correspond to live work. Unlike the lifecycle mutations it deliberately
    /// does not require the row to be the latest generation: a superseded
    /// occupying generation is, by definition, a lifecycle that already ended.
    pub async fn reclaim_absent_object(
        &self,
        input: &ReclaimAbsentInput,
    ) -> DbResult<ReclaimAbsentOutcome> {
        if input.observed_state == AdmissionState::Terminal {
            return Err(DbError::InvalidData(
                "reclamation evidence must describe an occupying state".into(),
            ));
        }
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        lock_work(&mut tx, input.key.domain, &input.key.work_id).await?;
        let Some(row) = fetch_row_for_update(&mut tx, &input.key).await? else {
            tx.commit().await?;
            return Ok(ReclaimAbsentOutcome::Fenced {
                reason: "admission row no longer exists".into(),
            });
        };
        if row.state == AdmissionState::Terminal {
            tx.commit().await?;
            return Ok(ReclaimAbsentOutcome::AlreadyTerminal(row));
        }
        if row.state != input.observed_state
            || row.creator_server_epoch != input.observed_creator_server_epoch
            || row.object_name != input.observed_object_name
            || row.object_uid != input.observed_object_uid
        {
            let reason = format!(
                "admission row changed after the absence proof (observed {:?}/{}, found {:?}/{})",
                input.observed_state, input.observed_object_name, row.state, row.object_name
            );
            tx.commit().await?;
            return Ok(ReclaimAbsentOutcome::Fenced { reason });
        }
        let reclaimed =
            update_state(&mut tx, &input.key, "terminal", row.object_uid.as_deref()).await?;
        tx.commit().await?;
        Ok(ReclaimAbsentOutcome::Reclaimed(reclaimed))
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

    /// Resolve the generation a dispatch attempt must reserve for `work_id`.
    ///
    /// A generation is one object lifecycle: exactly one create, at most one
    /// Kubernetes UID, one terminal release. A caller-supplied generation (a
    /// task's `reopen_count`, or the warm path's fixed generation) is therefore
    /// only a *floor*, never the identity itself — a second dispatch attempt at
    /// the same floor is a second object with its own UID and must not inherit
    /// the retired row's recorded UID.
    ///
    /// Resolution, under the same per-work advisory lock the mutations take:
    /// * no retained row — the requested generation, so first-ever dispatch
    ///   keeps the caller's numbering,
    /// * latest retained generation nonterminal — that generation, so a
    ///   duplicate dispatch or a restart resumes the in-flight row idempotently
    ///   instead of double-reserving capacity,
    /// * latest retained generation terminal — one past it, so a new attempt
    ///   always starts from a row with no object UID.
    ///
    /// The result is never below `requested`, keeping the journal generation
    /// aligned with a caller counter that advances faster than dispatch does.
    /// Allocation intentionally does not insert a row: callers must reserve the
    /// returned generation through [`Self::reserve`] or
    /// [`Self::reserve_observed`].
    pub async fn resolve_dispatch_generation(
        &self,
        domain: AdmissionDomain,
        work_id: &str,
        requested: i64,
    ) -> DbResult<i64> {
        if requested < 0 {
            return Err(DbError::InvalidData(
                "admission generation must be non-negative".into(),
            ));
        }
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
        tx.commit().await?;
        let resolved = match latest {
            None => requested,
            Some((generation, state)) => match AdmissionState::parse(&state)? {
                AdmissionState::Terminal => generation.saturating_add(1),
                _ => generation,
            },
        };
        Ok(resolved.max(requested))
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
struct SettlementDbRow {
    #[sqlx(flatten)]
    row: JournalDbRow,
    settled: Option<bool>,
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

/// Rows currently occupying the task-or-warm LIFECYCLE view.
///
/// This is no longer a capacity input -- nothing denies on it. It backs the
/// recovery/readiness surface (how much occupying lifecycle a restart inherited)
/// and telemetry. Real capacity is `BuildLeaseRepository`'s weighted sum.
#[allow(dead_code)]
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

/// Row-locked fetch of one exact generation, without the latest-generation
/// requirement [`current_row_for_update`] imposes on lifecycle mutations.
async fn fetch_row_for_update(
    tx: &mut Transaction<'_, Postgres>,
    key: &AdmissionJournalKey,
) -> DbResult<Option<AdmissionJournalRow>> {
    let row = sqlx::query_as::<_, JournalDbRow>(&format!(
        "SELECT {JOURNAL_COLUMNS} FROM admission_journal \
         WHERE domain = $1 AND work_id = $2 AND generation = $3 FOR UPDATE"
    ))
    .bind(key.domain.as_str())
    .bind(&key.work_id)
    .bind(key.generation)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(TryInto::try_into).transpose()
}

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
#[path = "admission_journal_tests.rs"]
mod tests;
