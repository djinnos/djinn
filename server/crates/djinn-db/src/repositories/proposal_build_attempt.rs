//! Persistence boundary for attempt-scoped proposal delivery.
//!
//! This module is deliberately the sole task-to-attempt ownership resolver.  It
//! keeps the dormant direct-delivery path from teaching each future consumer its
//! own (and inevitably divergent) interpretation of proposal ownership.

use std::str::FromStr;

use djinn_core::models::{
    DirectDeliveryParkReason, ProposalBuildAttempt, ProposalBuildAttemptLifecycle,
};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Postgres, Transaction};

use crate::database::Database;
use crate::error::{DbError, DbResult};
use crate::repositories::direct_delivery_capability::{
    DirectDeliveryCapabilityRepository, DirectDeliverySchemaCapability,
};

const ATTEMPT_COLUMNS: &str = "id, proposal_id, short_id, lifecycle, base_sha, branch_head_sha, branch_name, proposal_pr_number, proposal_pr_url, park_reason, \
    to_char(created_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS created_at, \
    to_char(activated_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS activated_at, \
    to_char(retired_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS retired_at";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReserveProposalBuildAttemptInput {
    pub proposal_id: String,
    pub proposal_short_id: String,
    pub build_attempt_id: String,
    pub build_attempt_short_id: String,
    /// The exact `main` SHA observed before expected-absent branch creation.
    pub observed_base_sha: String,
}

#[derive(FromRow)]
struct AttemptLeaseRow {
    build_attempt_id: String,
    owner_incarnation_id: String,
    generation: i64,
    expires_at: String,
}

impl From<AttemptLeaseRow> for ProposalBuildAttemptLease {
    fn from(row: AttemptLeaseRow) -> Self {
        Self {
            build_attempt_id: row.build_attempt_id,
            owner_incarnation_id: row.owner_incarnation_id,
            generation: row.generation,
            expires_at: row.expires_at,
        }
    }
}

impl ReserveProposalBuildAttemptInput {
    #[must_use]
    pub fn branch_name(&self) -> String {
        format!(
            "proposal/{}/{}",
            self.proposal_short_id, self.build_attempt_short_id
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReserveProposalBuildAttemptResult {
    Reserved(ProposalBuildAttempt),
    Replayed(ProposalBuildAttempt),
    CompetingIdentity { existing: ProposalBuildAttempt },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivateProposalBuildAttemptInput {
    pub build_attempt_id: String,
    pub expected_lifecycle: ProposalBuildAttemptLifecycle,
    pub expected_branch_head_sha: Option<String>,
    pub branch_head_sha: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivateProposalBuildAttemptResult {
    Activated(ProposalBuildAttempt),
    Replayed(ProposalBuildAttempt),
    Stale { current: ProposalBuildAttempt },
}

/// Persisted fencing lease for one build attempt.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProposalBuildAttemptLease {
    pub build_attempt_id: String,
    pub owner_incarnation_id: String,
    pub generation: i64,
    pub expires_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquireProposalBuildAttemptLeaseInput {
    pub build_attempt_id: String,
    pub owner_incarnation_id: String,
    /// Zero creates the first lease; takeover presents the observed token.
    pub expected_generation: i64,
    /// RFC3339 timestamp interpreted by PostgreSQL.
    pub expires_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AcquireProposalBuildAttemptLeaseResult {
    Acquired(ProposalBuildAttemptLease),
    Replayed(ProposalBuildAttemptLease),
    Stale {
        current: Option<ProposalBuildAttemptLease>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconcileAttemptBranchHeadInput {
    pub build_attempt_id: String,
    /// The local identity the caller read before observing the remote ref.
    pub expected_branch_head_sha: Option<String>,
    /// The exact remote ref head that is being reconciled.
    pub observed_branch_head_sha: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReconcileAttemptBranchHeadResult {
    Reconciled(ProposalBuildAttempt),
    Replayed(ProposalBuildAttempt),
    /// A concurrent writer changed the head after the caller's read. No identity
    /// is overwritten and no park decision is made from a stale observation.
    Stale {
        current: ProposalBuildAttempt,
    },
    Parked {
        attempt: ProposalBuildAttempt,
        reason: DirectDeliveryParkReason,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistAttemptPrIdentityInput {
    pub build_attempt_id: String,
    pub proposal_pr_number: i64,
    pub proposal_pr_url: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PersistAttemptPrIdentityResult {
    Persisted(ProposalBuildAttempt),
    Replayed(ProposalBuildAttempt),
    Parked {
        attempt: ProposalBuildAttempt,
        reason: DirectDeliveryParkReason,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetireProposalBuildAttemptInput {
    pub build_attempt_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum RetireProposalBuildAttemptResult {
    Retired(ProposalBuildAttempt),
    Replayed(ProposalBuildAttempt),
}

/// Failure modes deliberately expose ownership rather than making callers parse
/// an error string and accidentally fall back to legacy task-PR behaviour.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[allow(clippy::large_enum_variant)]
pub enum ResolveTaskActiveAttemptResult {
    Resolved(TaskActiveBuildAttempt),
    NoProposalOwner {
        task_id: String,
    },
    NoActiveAttempt {
        task_id: String,
        proposal_id: String,
    },
    AmbiguousProposalOwner {
        task_id: String,
        proposal_ids: Vec<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskActiveBuildAttempt {
    pub task_id: String,
    pub proposal_id: String,
    pub attempt: ProposalBuildAttempt,
}

#[derive(FromRow)]
struct AttemptRow {
    id: String,
    proposal_id: String,
    short_id: String,
    lifecycle: String,
    base_sha: String,
    branch_head_sha: Option<String>,
    branch_name: String,
    proposal_pr_number: Option<i64>,
    proposal_pr_url: Option<String>,
    park_reason: Option<String>,
    created_at: String,
    activated_at: Option<String>,
    retired_at: Option<String>,
}

impl TryFrom<AttemptRow> for ProposalBuildAttempt {
    type Error = DbError;

    fn try_from(row: AttemptRow) -> DbResult<Self> {
        Ok(Self {
            id: row.id,
            proposal_id: row.proposal_id,
            short_id: row.short_id,
            lifecycle: ProposalBuildAttemptLifecycle::from_str(&row.lifecycle)
                .map_err(DbError::InvalidData)?,
            base_sha: row.base_sha,
            branch_head_sha: row.branch_head_sha,
            branch_name: row.branch_name,
            proposal_pr_number: row.proposal_pr_number,
            proposal_pr_url: row.proposal_pr_url,
            park_reason: row
                .park_reason
                .map(|reason| {
                    DirectDeliveryParkReason::from_str(&reason).map_err(DbError::InvalidData)
                })
                .transpose()?,
            created_at: row.created_at,
            activated_at: row.activated_at,
            retired_at: row.retired_at,
        })
    }
}

/// Dark-deployment repository. Reservation is intentionally allowed while the
/// supported epoch is disabled so a later activation can adopt the exact branch
/// identity. Every other mutation requires the explicit active epoch.
pub struct ProposalBuildAttemptRepository {
    db: Database,
}

impl ProposalBuildAttemptRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    pub async fn reserve(
        &self,
        input: &ReserveProposalBuildAttemptInput,
    ) -> DbResult<ReserveProposalBuildAttemptResult> {
        self.require_capability(true).await?;
        validate_reservation(input)?;
        let mut tx = self.db.pool().begin().await?;
        lock_proposal(&mut tx, &input.proposal_id).await?;

        if let Some(existing) = fetch_attempt(&mut tx, &input.build_attempt_id, true).await? {
            tx.commit().await?;
            return Ok(if same_reservation(&existing, input) {
                ReserveProposalBuildAttemptResult::Replayed(existing)
            } else {
                ReserveProposalBuildAttemptResult::CompetingIdentity { existing }
            });
        }
        if let Some(existing) = fetch_attempt_by_proposal_short(&mut tx, input, true).await? {
            tx.commit().await?;
            return Ok(if same_reservation(&existing, input) {
                ReserveProposalBuildAttemptResult::Replayed(existing)
            } else {
                ReserveProposalBuildAttemptResult::CompetingIdentity { existing }
            });
        }
        let branch_name = input.branch_name();
        if let Some(existing) = fetch_attempt_by_branch(&mut tx, &branch_name, true).await? {
            tx.commit().await?;
            return Ok(ReserveProposalBuildAttemptResult::CompetingIdentity { existing });
        }

        let row = sqlx::query_as::<_, AttemptRow>(&format!(
            "INSERT INTO proposal_build_attempts (id, proposal_id, short_id, lifecycle, base_sha, branch_name) \
             VALUES ($1, $2, $3, 'reserved', $4, $5) RETURNING {ATTEMPT_COLUMNS}"
        ))
        .bind(&input.build_attempt_id)
        .bind(&input.proposal_id)
        .bind(&input.build_attempt_short_id)
        .bind(&input.observed_base_sha)
        .bind(branch_name)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(ReserveProposalBuildAttemptResult::Reserved(row.try_into()?))
    }

    pub async fn activate(
        &self,
        input: &ActivateProposalBuildAttemptInput,
    ) -> DbResult<ActivateProposalBuildAttemptResult> {
        self.require_capability(false).await?;
        require_nonblank("build_attempt_id", &input.build_attempt_id)?;
        require_nonblank("branch_head_sha", &input.branch_head_sha)?;
        let mut tx = self.db.pool().begin().await?;
        let Some(current) = fetch_attempt(&mut tx, &input.build_attempt_id, true).await? else {
            return Err(DbError::InvalidData(
                "unknown proposal build attempt".into(),
            ));
        };
        if current.lifecycle == ProposalBuildAttemptLifecycle::Active
            && current.branch_head_sha.as_deref() == Some(input.branch_head_sha.as_str())
        {
            tx.commit().await?;
            return Ok(ActivateProposalBuildAttemptResult::Replayed(current));
        }
        // `expected_lifecycle` remains in the wire shape so stale callers get
        // a typed result, but activation itself is only reserved -> active.
        if input.expected_lifecycle != ProposalBuildAttemptLifecycle::Reserved
            || current.lifecycle != ProposalBuildAttemptLifecycle::Reserved
            || current.branch_head_sha != input.expected_branch_head_sha
        {
            tx.commit().await?;
            return Ok(ActivateProposalBuildAttemptResult::Stale { current });
        }
        let row = sqlx::query_as::<_, AttemptRow>(&format!(
            "UPDATE proposal_build_attempts SET lifecycle = 'active', branch_head_sha = $1, activated_at = now() \
             WHERE id = $2 AND lifecycle = 'reserved' AND branch_head_sha IS NOT DISTINCT FROM $3 \
             RETURNING {ATTEMPT_COLUMNS}"
        ))
        .bind(&input.branch_head_sha)
        .bind(&input.build_attempt_id)
        .bind(&input.expected_branch_head_sha)
        .fetch_optional(&mut *tx)
        .await?;
        let result = match row {
            Some(row) => ActivateProposalBuildAttemptResult::Activated(row.try_into()?),
            None => ActivateProposalBuildAttemptResult::Stale {
                current: fetch_attempt(&mut tx, &input.build_attempt_id, true)
                    .await?
                    .ok_or_else(|| {
                        DbError::InvalidData("proposal build attempt disappeared".into())
                    })?,
            },
        };
        tx.commit().await?;
        Ok(result)
    }

    /// Acquire an attempt-wide fencing lease. A different owner can take over
    /// only an expired lease with the exact generation it observed.
    pub async fn acquire_lease(
        &self,
        input: &AcquireProposalBuildAttemptLeaseInput,
    ) -> DbResult<AcquireProposalBuildAttemptLeaseResult> {
        self.require_capability(false).await?;
        require_nonblank("build_attempt_id", &input.build_attempt_id)?;
        require_nonblank("owner_incarnation_id", &input.owner_incarnation_id)?;
        require_nonblank("expires_at", &input.expires_at)?;
        if input.expected_generation < 0 {
            return Err(DbError::InvalidData(
                "expected_generation must not be negative".into(),
            ));
        }
        // A request presents the generation it observed; acquisition persists
        // its successor. Derive it once without risking a SQL-side overflow.
        let acquired_generation = input.expected_generation.checked_add(1).ok_or_else(|| {
            DbError::InvalidData("expected_generation cannot be incremented".into())
        })?;
        let mut tx = self.db.pool().begin().await?;
        // Lease rows use this canonical UTC/millisecond representation. Normalize
        // the request with PostgreSQL before comparing it for crash-retry replay.
        let normalized_expires_at: String = sqlx::query_scalar(
            "SELECT to_char($1::timestamptz AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')",
        )
        .bind(&input.expires_at)
        .fetch_one(&mut *tx)
        .await?;
        let Some(attempt) = fetch_attempt(&mut tx, &input.build_attempt_id, true).await? else {
            return Err(DbError::InvalidData(
                "unknown proposal build attempt".into(),
            ));
        };
        if attempt.lifecycle == ProposalBuildAttemptLifecycle::Retired {
            tx.commit().await?;
            return Ok(AcquireProposalBuildAttemptLeaseResult::Stale { current: None });
        }
        let current = fetch_attempt_lease(&mut tx, &input.build_attempt_id).await?;
        if let Some(ref lease) = current
            && lease.owner_incarnation_id == input.owner_incarnation_id
            && lease.generation == acquired_generation
            && lease.expires_at == normalized_expires_at
        {
            tx.commit().await?;
            return Ok(AcquireProposalBuildAttemptLeaseResult::Replayed(
                lease.clone(),
            ));
        }
        let acquired = match current {
            None if input.expected_generation == 0 => sqlx::query_as::<_, AttemptLeaseRow>(
                "INSERT INTO proposal_build_attempt_leases (build_attempt_id, owner_incarnation_id, generation, expires_at) VALUES ($1, $2, $3, $4::timestamptz) RETURNING build_attempt_id, owner_incarnation_id, generation, to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS expires_at",
            ).bind(&input.build_attempt_id).bind(&input.owner_incarnation_id).bind(acquired_generation).bind(&normalized_expires_at).fetch_optional(&mut *tx).await?,
            Some(lease) if lease.generation == input.expected_generation => sqlx::query_as::<_, AttemptLeaseRow>(
                "UPDATE proposal_build_attempt_leases SET owner_incarnation_id = $1, generation = $2, expires_at = $3::timestamptz WHERE build_attempt_id = $4 AND generation = $5 AND expires_at <= now() RETURNING build_attempt_id, owner_incarnation_id, generation, to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS expires_at",
            ).bind(&input.owner_incarnation_id).bind(acquired_generation).bind(&normalized_expires_at).bind(&input.build_attempt_id).bind(lease.generation).fetch_optional(&mut *tx).await?,
            _ => None,
        };
        let result = match acquired {
            Some(lease) => AcquireProposalBuildAttemptLeaseResult::Acquired(lease.into()),
            None => AcquireProposalBuildAttemptLeaseResult::Stale {
                current: fetch_attempt_lease(&mut tx, &input.build_attempt_id).await?,
            },
        };
        tx.commit().await?;
        Ok(result)
    }

    pub async fn reconcile_branch_head(
        &self,
        input: &ReconcileAttemptBranchHeadInput,
    ) -> DbResult<ReconcileAttemptBranchHeadResult> {
        self.require_capability(false).await?;
        require_nonblank("build_attempt_id", &input.build_attempt_id)?;
        require_nonblank("observed_branch_head_sha", &input.observed_branch_head_sha)?;
        let mut tx = self.db.pool().begin().await?;
        let Some(current) = fetch_attempt(&mut tx, &input.build_attempt_id, true).await? else {
            return Err(DbError::InvalidData(
                "unknown proposal build attempt".into(),
            ));
        };
        if current.branch_head_sha.as_deref() == Some(input.observed_branch_head_sha.as_str()) {
            tx.commit().await?;
            return Ok(ReconcileAttemptBranchHeadResult::Replayed(current));
        }
        if current.branch_head_sha != input.expected_branch_head_sha {
            tx.commit().await?;
            return Ok(ReconcileAttemptBranchHeadResult::Stale { current });
        }
        if current.lifecycle == ProposalBuildAttemptLifecycle::Retired {
            tx.commit().await?;
            return Ok(ReconcileAttemptBranchHeadResult::Stale { current });
        }
        // The first observation must prove that expected-absent ref creation
        // started at the exact base recorded at reservation. Otherwise a
        // pre-existing ref was adopted, so retain the absent local identity and
        // park instead of installing its head.
        if current.branch_head_sha.is_none() && input.observed_branch_head_sha != current.base_sha {
            let attempt = park_tx(
                &mut tx,
                &current.id,
                DirectDeliveryParkReason::BranchIdentityMismatch,
            )
            .await?;
            tx.commit().await?;
            return Ok(ReconcileAttemptBranchHeadResult::Parked {
                attempt,
                reason: DirectDeliveryParkReason::BranchIdentityMismatch,
            });
        }
        // An already-published branch is immutable from this repository's point
        // of view. A different observed head is evidence, not permission to
        // overwrite its identity.
        if current.branch_head_sha.is_some() {
            let attempt = park_tx(
                &mut tx,
                &current.id,
                DirectDeliveryParkReason::UnexpectedBranchHead,
            )
            .await?;
            tx.commit().await?;
            return Ok(ReconcileAttemptBranchHeadResult::Parked {
                attempt,
                reason: DirectDeliveryParkReason::UnexpectedBranchHead,
            });
        }
        let row = sqlx::query_as::<_, AttemptRow>(&format!(
            "UPDATE proposal_build_attempts SET branch_head_sha = $1 \
             WHERE id = $2 AND lifecycle <> 'retired' AND branch_head_sha IS NOT DISTINCT FROM $3 RETURNING {ATTEMPT_COLUMNS}"
        ))
        .bind(&input.observed_branch_head_sha)
        .bind(&input.build_attempt_id)
        .bind(&input.expected_branch_head_sha)
        .fetch_optional(&mut *tx)
        .await?;
        let result = match row {
            Some(row) => ReconcileAttemptBranchHeadResult::Reconciled(row.try_into()?),
            None => ReconcileAttemptBranchHeadResult::Stale {
                current: fetch_attempt(&mut tx, &input.build_attempt_id, true)
                    .await?
                    .ok_or_else(|| {
                        DbError::InvalidData("proposal build attempt disappeared".into())
                    })?,
            },
        };
        tx.commit().await?;
        Ok(result)
    }

    pub async fn persist_pr_identity(
        &self,
        input: &PersistAttemptPrIdentityInput,
    ) -> DbResult<PersistAttemptPrIdentityResult> {
        self.require_capability(false).await?;
        require_nonblank("build_attempt_id", &input.build_attempt_id)?;
        require_nonblank("proposal_pr_url", &input.proposal_pr_url)?;
        if input.proposal_pr_number <= 0 {
            return Err(DbError::InvalidData(
                "proposal_pr_number must be positive".into(),
            ));
        }
        let mut tx = self.db.pool().begin().await?;
        let Some(current) = fetch_attempt(&mut tx, &input.build_attempt_id, true).await? else {
            return Err(DbError::InvalidData(
                "unknown proposal build attempt".into(),
            ));
        };
        let existing_pr: Option<(Option<i64>, Option<String>)> = sqlx::query_as(
            "SELECT proposal_pr_number, proposal_pr_url FROM proposal_build_attempts WHERE id = $1 FOR UPDATE",
        )
        .bind(&input.build_attempt_id)
        .fetch_optional(&mut *tx)
        .await?;
        let (number, url) = existing_pr.expect("attempt was locked above");
        if number == Some(input.proposal_pr_number)
            && url.as_deref() == Some(input.proposal_pr_url.as_str())
        {
            tx.commit().await?;
            return Ok(PersistAttemptPrIdentityResult::Replayed(current));
        }
        if current.lifecycle == ProposalBuildAttemptLifecycle::Retired {
            tx.commit().await?;
            return Ok(PersistAttemptPrIdentityResult::Parked {
                attempt: current,
                reason: DirectDeliveryParkReason::ProposalPrIdentityMismatch,
            });
        }
        if number.is_some() || url.is_some() {
            let attempt = park_tx(
                &mut tx,
                &current.id,
                DirectDeliveryParkReason::ProposalPrIdentityMismatch,
            )
            .await?;
            tx.commit().await?;
            return Ok(PersistAttemptPrIdentityResult::Parked {
                attempt,
                reason: DirectDeliveryParkReason::ProposalPrIdentityMismatch,
            });
        }
        let row = sqlx::query_as::<_, AttemptRow>(&format!(
            "UPDATE proposal_build_attempts SET proposal_pr_number = $1, proposal_pr_url = $2 \
             WHERE id = $3 AND lifecycle <> 'retired' AND proposal_pr_number IS NULL AND proposal_pr_url IS NULL RETURNING {ATTEMPT_COLUMNS}"
        ))
        .bind(input.proposal_pr_number)
        .bind(&input.proposal_pr_url)
        .bind(&input.build_attempt_id)
        .fetch_optional(&mut *tx)
        .await?;
        let result = match row {
            Some(row) => PersistAttemptPrIdentityResult::Persisted(row.try_into()?),
            None => {
                let attempt = park_tx(
                    &mut tx,
                    &current.id,
                    DirectDeliveryParkReason::ProposalPrIdentityMismatch,
                )
                .await?;
                PersistAttemptPrIdentityResult::Parked {
                    attempt,
                    reason: DirectDeliveryParkReason::ProposalPrIdentityMismatch,
                }
            }
        };
        tx.commit().await?;
        Ok(result)
    }

    pub async fn retire(
        &self,
        input: &RetireProposalBuildAttemptInput,
    ) -> DbResult<RetireProposalBuildAttemptResult> {
        self.require_capability(false).await?;
        require_nonblank("build_attempt_id", &input.build_attempt_id)?;
        let mut tx = self.db.pool().begin().await?;
        let Some(current) = fetch_attempt(&mut tx, &input.build_attempt_id, true).await? else {
            return Err(DbError::InvalidData(
                "unknown proposal build attempt".into(),
            ));
        };
        if current.lifecycle == ProposalBuildAttemptLifecycle::Retired {
            tx.commit().await?;
            return Ok(RetireProposalBuildAttemptResult::Replayed(current));
        }
        let row = sqlx::query_as::<_, AttemptRow>(&format!(
            "UPDATE proposal_build_attempts SET lifecycle = 'retired', retired_at = now() \
             WHERE id = $1 AND lifecycle <> 'retired' RETURNING {ATTEMPT_COLUMNS}"
        ))
        .bind(&input.build_attempt_id)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(RetireProposalBuildAttemptResult::Retired(row.try_into()?))
    }

    /// Persist the first terminal orchestration observation without mutating
    /// branch, PR, or retirement identity.
    pub async fn park(
        &self,
        build_attempt_id: &str,
        reason: DirectDeliveryParkReason,
    ) -> DbResult<ProposalBuildAttempt> {
        self.require_capability(false).await?;
        require_nonblank("build_attempt_id", build_attempt_id)?;
        let mut tx = self.db.pool().begin().await?;
        let Some(current) = fetch_attempt(&mut tx, build_attempt_id, true).await? else {
            return Err(DbError::InvalidData(
                "unknown proposal build attempt".into(),
            ));
        };
        if current.lifecycle == ProposalBuildAttemptLifecycle::Retired {
            tx.commit().await?;
            return Ok(current);
        }
        let parked = park_tx(&mut tx, build_attempt_id, reason).await?;
        tx.commit().await?;
        Ok(parked)
    }

    /// Canonical task ownership route: `tasks.epic_id -> epics.proposal_id`.
    /// The breakdown fallback is intentionally joined only when `epic_id IS
    /// NULL`, so it can resolve exactly that otherwise-epic-less task and no
    /// ordinary task can borrow a proposal through it.
    pub async fn resolve_task_active_attempt(
        &self,
        task_id: &str,
    ) -> DbResult<ResolveTaskActiveAttemptResult> {
        self.require_capability(true).await?;
        require_nonblank("task_id", task_id)?;
        let proposal_ids: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT COALESCE(e.proposal_id, fallback.id) \
             FROM tasks t \
             LEFT JOIN epics e ON e.id = t.epic_id \
             LEFT JOIN proposals fallback ON t.epic_id IS NULL AND fallback.build_breakdown_task_id = t.id \
             WHERE t.id = $1 AND COALESCE(e.proposal_id, fallback.id) IS NOT NULL \
             ORDER BY COALESCE(e.proposal_id, fallback.id)",
        )
        .bind(task_id)
        .fetch_all(self.db.pool())
        .await?;
        match proposal_ids.as_slice() {
            [] => Ok(ResolveTaskActiveAttemptResult::NoProposalOwner {
                task_id: task_id.into(),
            }),
            [proposal_id] => {
                let attempt = fetch_active_attempt(self.db.pool(), proposal_id).await?;
                Ok(match attempt {
                    Some(attempt) => {
                        ResolveTaskActiveAttemptResult::Resolved(TaskActiveBuildAttempt {
                            task_id: task_id.into(),
                            proposal_id: proposal_id.clone(),
                            attempt,
                        })
                    }
                    None => ResolveTaskActiveAttemptResult::NoActiveAttempt {
                        task_id: task_id.into(),
                        proposal_id: proposal_id.clone(),
                    },
                })
            }
            _ => Ok(ResolveTaskActiveAttemptResult::AmbiguousProposalOwner {
                task_id: task_id.into(),
                proposal_ids,
            }),
        }
    }

    async fn require_capability(&self, allow_disabled: bool) -> DbResult<()> {
        match DirectDeliveryCapabilityRepository::new(self.db.clone())
            .probe()
            .await?
        {
            DirectDeliverySchemaCapability::SupportedActive { .. } => Ok(()),
            DirectDeliverySchemaCapability::SupportedDisabled { .. } if allow_disabled => Ok(()),
            DirectDeliverySchemaCapability::SupportedDisabled { .. } => Err(
                DbError::InvalidTransition("direct_delivery_v1 epoch is disabled".into()),
            ),
            DirectDeliverySchemaCapability::MissingSchema { missing_relations } => {
                Err(DbError::InvalidData(format!(
                    "direct_delivery_v1 schema unavailable: {}",
                    missing_relations.join(", ")
                )))
            }
            DirectDeliverySchemaCapability::MissingEpoch => Err(DbError::InvalidData(
                "direct_delivery_v1 epoch is unavailable".into(),
            )),
            DirectDeliverySchemaCapability::UnknownEpochState { state, generation } => {
                Err(DbError::InvalidData(format!(
                    "direct_delivery_v1 has unknown state {state} at generation {generation}"
                )))
            }
        }
    }
}

fn validate_reservation(input: &ReserveProposalBuildAttemptInput) -> DbResult<()> {
    require_nonblank("proposal_id", &input.proposal_id)?;
    require_nonblank("proposal_short_id", &input.proposal_short_id)?;
    require_nonblank("build_attempt_id", &input.build_attempt_id)?;
    require_nonblank("build_attempt_short_id", &input.build_attempt_short_id)?;
    require_nonblank("observed_base_sha", &input.observed_base_sha)
}

fn require_nonblank(field: &str, value: &str) -> DbResult<()> {
    if value.trim().is_empty() {
        return Err(DbError::InvalidData(format!("{field} must not be blank")));
    }
    Ok(())
}

fn same_reservation(
    existing: &ProposalBuildAttempt,
    input: &ReserveProposalBuildAttemptInput,
) -> bool {
    existing.id == input.build_attempt_id
        && existing.proposal_id == input.proposal_id
        && existing.short_id == input.build_attempt_short_id
        && existing.base_sha == input.observed_base_sha
        && existing.branch_name == input.branch_name()
}

async fn lock_proposal(tx: &mut Transaction<'_, Postgres>, proposal_id: &str) -> DbResult<()> {
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(format!("proposal-build-attempt:{proposal_id}"))
        .execute(&mut **tx)
        .await?;
    Ok(())
}

async fn fetch_attempt(
    tx: &mut Transaction<'_, Postgres>,
    id: &str,
    for_update: bool,
) -> DbResult<Option<ProposalBuildAttempt>> {
    let suffix = if for_update { " FOR UPDATE" } else { "" };
    let row = sqlx::query_as::<_, AttemptRow>(&format!(
        "SELECT {ATTEMPT_COLUMNS} FROM proposal_build_attempts WHERE id = $1{suffix}"
    ))
    .bind(id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(TryInto::try_into).transpose()
}

async fn fetch_attempt_lease(
    tx: &mut Transaction<'_, Postgres>,
    build_attempt_id: &str,
) -> DbResult<Option<ProposalBuildAttemptLease>> {
    let row = sqlx::query_as::<_, AttemptLeaseRow>(
        "SELECT build_attempt_id, owner_incarnation_id, generation, \
         to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS expires_at \
         FROM proposal_build_attempt_leases WHERE build_attempt_id = $1 FOR UPDATE",
    )
    .bind(build_attempt_id)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(Into::into))
}

async fn fetch_attempt_by_proposal_short(
    tx: &mut Transaction<'_, Postgres>,
    input: &ReserveProposalBuildAttemptInput,
    for_update: bool,
) -> DbResult<Option<ProposalBuildAttempt>> {
    let suffix = if for_update { " FOR UPDATE" } else { "" };
    let row = sqlx::query_as::<_, AttemptRow>(&format!(
        "SELECT {ATTEMPT_COLUMNS} FROM proposal_build_attempts WHERE proposal_id = $1 AND short_id = $2{suffix}"
    ))
    .bind(&input.proposal_id)
    .bind(&input.build_attempt_short_id)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(TryInto::try_into).transpose()
}

async fn fetch_attempt_by_branch(
    tx: &mut Transaction<'_, Postgres>,
    branch_name: &str,
    for_update: bool,
) -> DbResult<Option<ProposalBuildAttempt>> {
    let suffix = if for_update { " FOR UPDATE" } else { "" };
    let row = sqlx::query_as::<_, AttemptRow>(&format!(
        "SELECT {ATTEMPT_COLUMNS} FROM proposal_build_attempts WHERE branch_name = $1{suffix}"
    ))
    .bind(branch_name)
    .fetch_optional(&mut **tx)
    .await?;
    row.map(TryInto::try_into).transpose()
}

async fn fetch_active_attempt(
    pool: &sqlx::PgPool,
    proposal_id: &str,
) -> DbResult<Option<ProposalBuildAttempt>> {
    let row = sqlx::query_as::<_, AttemptRow>(&format!(
        "SELECT {ATTEMPT_COLUMNS} FROM proposal_build_attempts WHERE proposal_id = $1 AND lifecycle = 'active'"
    ))
    .bind(proposal_id)
    .fetch_optional(pool)
    .await?;
    row.map(TryInto::try_into).transpose()
}

async fn park_tx(
    tx: &mut Transaction<'_, Postgres>,
    build_attempt_id: &str,
    reason: DirectDeliveryParkReason,
) -> DbResult<ProposalBuildAttempt> {
    let row = sqlx::query_as::<_, AttemptRow>(&format!(
        "UPDATE proposal_build_attempts SET park_reason = COALESCE(park_reason, $1) WHERE id = $2 AND lifecycle <> 'retired' RETURNING {ATTEMPT_COLUMNS}"
    ))
    .bind(reason.as_str())
    .bind(build_attempt_id)
    .fetch_one(&mut **tx)
    .await?;
    row.try_into()
}
