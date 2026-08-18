//! C4: the `direct_delivery_v1` activation fence.
//!
//! This module owns the **only** non-test statement in the tree that sets
//! `direct_delivery_epochs.state = 'active'`. Everything the proposal's C4 row
//! demands happens inside a single transaction:
//!
//! * the additive schema is present (all six C0 relations);
//! * the epoch row exists, parses, and is still `disabled`;
//! * the caller's observed generation still matches (competing/stale activation);
//! * every **live** process has advertised all five
//!   [`DirectDeliveryCapability`] values at the target generation — the full
//!   live-process census;
//! * no unexpired, unreleased [`direct_delivery_leases`] row carries a
//!   *legacy* (`< target`) epoch generation;
//! * only then does the compare-and-set `disabled -> active` run.
//!
//! Each prerequisite is checked in its own step and reported as its own typed
//! refusal, so removing exactly one of them refuses activation on exactly that
//! ground and leaves the epoch disabled.
//!
//! It also owns the delivery-lease writer/reader pair. A lease binds one
//! in-flight `(build_attempt_id, task_id, delivery_generation)` mutation to the
//! epoch generation its owner probed; acquisition at a generation *older* than
//! the persisted epoch is rejected, which is what makes a post-activation stale
//! process unable to keep writing.

use std::fmt;
use std::str::FromStr;

use djinn_core::models::{
    DirectDeliveryCapability, DirectDeliveryCapabilityRecord, DirectDeliveryEpoch,
    DirectDeliveryEpochState, DirectDeliveryLease, TaskDeliveryIdentity,
};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Postgres, Transaction};

use crate::database::Database;
use crate::error::{DbError, DbResult};

/// Relations migration 203/204 add. Activation refuses before touching any of
/// them if even one is absent, so a new binary against an old schema cannot
/// half-activate.
const REQUIRED_RELATIONS: [&str; 6] = [
    "proposal_build_attempts",
    "direct_delivery_epochs",
    "direct_delivery_process_capabilities",
    "task_deliveries",
    "direct_delivery_leases",
    "proposal_build_attempt_leases",
];

const LEASE_COLUMNS: &str = "id, build_attempt_id, task_id, delivery_generation, owner_incarnation_id, epoch_generation, \
    to_char(acquired_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS acquired_at, \
    to_char(expires_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"') AS expires_at";

/// One live process and the capabilities it has **not** advertised at the
/// target generation. A process that has advertised nothing at all appears here
/// with every capability missing — that is the "stale process replay" case.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapabilityCensusGap {
    pub process_incarnation_id: String,
    pub missing: Vec<DirectDeliveryCapability>,
}

impl fmt::Display for CapabilityCensusGap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} missing [", self.process_incarnation_id)?;
        for (index, capability) in self.missing.iter().enumerate() {
            if index > 0 {
                f.write_str(", ")?;
            }
            f.write_str(capability.as_str())?;
        }
        f.write_str("]")
    }
}

/// Why activation left the epoch disabled. Every variant is a refusal, never a
/// partial application: each refusal path reaches its `return` before the
/// compare-and-set, so the transaction it commits contains no write at all.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DirectDeliveryActivationRefusal {
    MissingSchema {
        missing_relations: Vec<String>,
    },
    MissingEpoch,
    UnknownEpochState {
        state: String,
        generation: i64,
    },
    /// Already `active`. Activation is not re-entrant at a different
    /// generation, and downgrade is unsupported.
    AlreadyActive {
        generation: i64,
    },
    /// The persisted generation is not the one the caller observed — a
    /// competing activation won, or this caller replayed a stale plan.
    CompetingGeneration {
        observed: i64,
        persisted: i64,
    },
    /// A census over zero processes is not a full census.
    NoLiveProcesses,
    IncompleteCapabilityCensus {
        gaps: Vec<CapabilityCensusGap>,
    },
    LiveLegacyDeliveryLeases {
        leases: Vec<DirectDeliveryLease>,
    },
}

impl DirectDeliveryActivationRefusal {
    /// Stable machine-readable discriminator for logs and assertions.
    #[must_use]
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::MissingSchema { .. } => "missing_schema",
            Self::MissingEpoch => "missing_epoch",
            Self::UnknownEpochState { .. } => "unknown_epoch_state",
            Self::AlreadyActive { .. } => "already_active",
            Self::CompetingGeneration { .. } => "competing_generation",
            Self::NoLiveProcesses => "no_live_processes",
            Self::IncompleteCapabilityCensus { .. } => "incomplete_capability_census",
            Self::LiveLegacyDeliveryLeases { .. } => "live_legacy_delivery_leases",
        }
    }
}

impl fmt::Display for DirectDeliveryActivationRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingSchema { missing_relations } => {
                write!(f, "missing_schema: {}", missing_relations.join(", "))
            }
            Self::MissingEpoch => f.write_str("missing_epoch"),
            Self::UnknownEpochState { state, generation } => {
                write!(f, "unknown_epoch_state: {state} at generation {generation}")
            }
            Self::AlreadyActive { generation } => {
                write!(f, "already_active at generation {generation}")
            }
            Self::CompetingGeneration {
                observed,
                persisted,
            } => write!(
                f,
                "competing_generation: observed {observed}, persisted {persisted}"
            ),
            Self::NoLiveProcesses => f.write_str("no_live_processes"),
            Self::IncompleteCapabilityCensus { gaps } => {
                f.write_str("incomplete_capability_census: ")?;
                for (index, gap) in gaps.iter().enumerate() {
                    if index > 0 {
                        f.write_str("; ")?;
                    }
                    write!(f, "{gap}")?;
                }
                Ok(())
            }
            Self::LiveLegacyDeliveryLeases { leases } => write!(
                f,
                "live_legacy_delivery_leases: {}",
                leases
                    .iter()
                    .map(|lease| format!("{}@generation {}", lease.id, lease.epoch_generation))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActivateDirectDeliveryEpochResult {
    Activated(DirectDeliveryEpoch),
    /// The epoch is already active at exactly the generation this call would
    /// have produced: a crash-retry of the same activation.
    Replayed(DirectDeliveryEpoch),
    Refused(DirectDeliveryActivationRefusal),
}

impl ActivateDirectDeliveryEpochResult {
    #[must_use]
    pub const fn refusal(&self) -> Option<&DirectDeliveryActivationRefusal> {
        match self {
            Self::Refused(refusal) => Some(refusal),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivateDirectDeliveryEpochInput {
    /// The generation the caller read before assembling its plan. Activation
    /// moves the epoch to `expected_generation + 1`.
    pub expected_generation: i64,
    /// Census liveness threshold: `coordinator_incarnations` rows whose
    /// `last_renewed_at` is at or after this ISO instant are live. Supplying it
    /// keeps the fence's liveness definition identical to the reaper's.
    pub live_since: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AcquireDeliveryLeaseInput {
    pub lease_id: String,
    pub identity: TaskDeliveryIdentity,
    pub owner_incarnation_id: String,
    /// The epoch generation the owner probed before deciding to mutate.
    pub epoch_generation: i64,
    /// RFC3339 instant interpreted by PostgreSQL.
    pub expires_at: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AcquireDeliveryLeaseResult {
    Acquired(DirectDeliveryLease),
    /// This owner already holds the live lease; its expiry was extended.
    Replayed(DirectDeliveryLease),
    /// A different owner holds an unexpired lease on the same generation.
    Held {
        current: DirectDeliveryLease,
    },
    /// The owner probed an epoch generation older than the persisted one: a
    /// later activation has fenced it out.
    StaleGeneration {
        requested: i64,
        persisted: i64,
    },
}

#[derive(FromRow)]
struct LeaseRow {
    id: String,
    build_attempt_id: String,
    task_id: String,
    delivery_generation: i64,
    owner_incarnation_id: String,
    epoch_generation: i64,
    acquired_at: String,
    expires_at: String,
}

impl From<LeaseRow> for DirectDeliveryLease {
    fn from(row: LeaseRow) -> Self {
        Self {
            id: row.id,
            build_attempt_id: row.build_attempt_id,
            task_id: row.task_id,
            delivery_generation: row.delivery_generation,
            owner_incarnation_id: row.owner_incarnation_id,
            epoch_generation: row.epoch_generation,
            acquired_at: row.acquired_at,
            expires_at: row.expires_at,
        }
    }
}

/// Writer and reader for `direct_delivery_process_capabilities`,
/// `direct_delivery_leases`, and the single activation compare-and-set.
pub struct DirectDeliveryActivationRepository {
    db: Database,
}

impl DirectDeliveryActivationRepository {
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Record what this process can do, at the generation it would activate
    /// into. Re-advertising the same process is an upsert, so a restarted or
    /// re-ticking process refreshes rather than duplicating.
    ///
    /// This is the sole production writer of
    /// `direct_delivery_process_capabilities`.
    pub async fn advertise_capabilities(
        &self,
        process_incarnation_id: &str,
        epoch_generation: i64,
        capabilities: &[DirectDeliveryCapability],
    ) -> DbResult<Vec<DirectDeliveryCapabilityRecord>> {
        self.db.ensure_initialized().await?;
        require_nonblank("process_incarnation_id", process_incarnation_id)?;
        if epoch_generation < 0 {
            return Err(DbError::InvalidData(
                "epoch_generation must not be negative".into(),
            ));
        }
        let mut records = Vec::with_capacity(capabilities.len());
        for capability in capabilities {
            let row: (String, String, i64, String) = sqlx::query_as(
                "INSERT INTO direct_delivery_process_capabilities \
                   (process_incarnation_id, capability, epoch_generation, observed_at) \
                 VALUES ($1, $2, $3, now()) \
                 ON CONFLICT (process_incarnation_id, capability) DO UPDATE \
                   SET epoch_generation = EXCLUDED.epoch_generation, observed_at = EXCLUDED.observed_at \
                 RETURNING process_incarnation_id, capability, epoch_generation, \
                   to_char(observed_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.MS\"Z\"')",
            )
            .bind(process_incarnation_id)
            .bind(capability.as_str())
            .bind(epoch_generation)
            .fetch_one(self.db.pool())
            .await?;
            records.push(DirectDeliveryCapabilityRecord {
                process_incarnation_id: row.0,
                capability: row.1.parse().map_err(|e: String| DbError::InvalidData(e))?,
                epoch_generation: row.2,
                observed_at: row.3,
            });
        }
        Ok(records)
    }

    /// Read the census gaps without attempting activation. Diagnostic reader of
    /// `direct_delivery_process_capabilities`; the activation transaction runs
    /// the same query under the epoch row lock.
    pub async fn capability_census_gaps(
        &self,
        target_generation: i64,
        live_since: &str,
    ) -> DbResult<Vec<CapabilityCensusGap>> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let gaps = capability_census_gaps(&mut tx, target_generation, live_since).await?;
        tx.commit().await?;
        Ok(gaps)
    }

    /// Every unreleased, unexpired lease whose owner probed a generation older
    /// than `target_generation`. Production reader of `direct_delivery_leases`.
    pub async fn live_legacy_delivery_leases(
        &self,
        target_generation: i64,
    ) -> DbResult<Vec<DirectDeliveryLease>> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let leases = live_legacy_delivery_leases(&mut tx, target_generation).await?;
        tx.commit().await?;
        Ok(leases)
    }

    /// Fence one in-flight delivery generation to one owner at one epoch
    /// generation. Production writer of `direct_delivery_leases`.
    pub async fn acquire_delivery_lease(
        &self,
        input: &AcquireDeliveryLeaseInput,
    ) -> DbResult<AcquireDeliveryLeaseResult> {
        self.db.ensure_initialized().await?;
        require_nonblank("lease_id", &input.lease_id)?;
        require_nonblank("owner_incarnation_id", &input.owner_incarnation_id)?;
        require_nonblank("expires_at", &input.expires_at)?;
        input
            .identity
            .validate()
            .map_err(|e| DbError::InvalidData(e.to_string()))?;
        if input.epoch_generation < 0 {
            return Err(DbError::InvalidData(
                "epoch_generation must not be negative".into(),
            ));
        }

        let mut tx = self.db.pool().begin().await?;
        // Serialize acquisitions for this exact generation. The partial unique
        // index only forbids two *live* rows; the lock is what stops two owners
        // from both reading "no live lease" and both inserting.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!(
                "direct-delivery-lease:{}:{}:{}",
                input.identity.build_attempt_id,
                input.identity.task_id,
                input.identity.delivery_generation
            ))
            .execute(&mut *tx)
            .await?;

        // A lease is only meaningful relative to the persisted epoch. An owner
        // presenting a generation older than the epoch has been fenced out by a
        // later activation and must not mutate anything.
        let persisted_generation: Option<i64> =
            sqlx::query_scalar("SELECT generation FROM direct_delivery_epochs WHERE name = $1")
                .bind(DirectDeliveryEpoch::NAME)
                .fetch_optional(&mut *tx)
                .await?;
        let persisted_generation = persisted_generation.ok_or_else(|| {
            DbError::InvalidData("direct_delivery_v1 epoch is unavailable".into())
        })?;
        if input.epoch_generation < persisted_generation {
            tx.commit().await?;
            return Ok(AcquireDeliveryLeaseResult::StaleGeneration {
                requested: input.epoch_generation,
                persisted: persisted_generation,
            });
        }

        let current = fetch_live_lease(&mut tx, &input.identity).await?;
        if let Some(existing) = current {
            if existing.owner_incarnation_id == input.owner_incarnation_id {
                let refreshed = sqlx::query_as::<_, LeaseRow>(&format!(
                    "UPDATE direct_delivery_leases SET expires_at = $1::timestamptz \
                     WHERE id = $2 RETURNING {LEASE_COLUMNS}"
                ))
                .bind(&input.expires_at)
                .bind(&existing.id)
                .fetch_one(&mut *tx)
                .await?;
                tx.commit().await?;
                return Ok(AcquireDeliveryLeaseResult::Replayed(refreshed.into()));
            }
            let expired: bool = sqlx::query_scalar(
                "SELECT expires_at <= now() FROM direct_delivery_leases WHERE id = $1",
            )
            .bind(&existing.id)
            .fetch_one(&mut *tx)
            .await?;
            if !expired {
                tx.commit().await?;
                return Ok(AcquireDeliveryLeaseResult::Held { current: existing });
            }
            // Takeover of an expired lease releases the old row rather than
            // rewriting its owner, so the handover stays inspectable.
            sqlx::query("UPDATE direct_delivery_leases SET released_at = now() WHERE id = $1")
                .bind(&existing.id)
                .execute(&mut *tx)
                .await?;
        }

        let acquired = sqlx::query_as::<_, LeaseRow>(&format!(
            "INSERT INTO direct_delivery_leases \
               (id, build_attempt_id, task_id, delivery_generation, owner_incarnation_id, epoch_generation, expires_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7::timestamptz) RETURNING {LEASE_COLUMNS}"
        ))
        .bind(&input.lease_id)
        .bind(&input.identity.build_attempt_id)
        .bind(&input.identity.task_id)
        .bind(input.identity.delivery_generation)
        .bind(&input.owner_incarnation_id)
        .bind(input.epoch_generation)
        .bind(&input.expires_at)
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(AcquireDeliveryLeaseResult::Acquired(acquired.into()))
    }

    /// Release the live lease this owner holds over `identity`. Returns whether
    /// a row moved. Production writer of `direct_delivery_leases`.
    pub async fn release_delivery_lease(
        &self,
        identity: &TaskDeliveryIdentity,
        owner_incarnation_id: &str,
    ) -> DbResult<bool> {
        self.db.ensure_initialized().await?;
        require_nonblank("owner_incarnation_id", owner_incarnation_id)?;
        identity
            .validate()
            .map_err(|e| DbError::InvalidData(e.to_string()))?;
        let result = sqlx::query(
            "UPDATE direct_delivery_leases SET released_at = now() \
             WHERE build_attempt_id = $1 AND task_id = $2 AND delivery_generation = $3 \
               AND owner_incarnation_id = $4 AND released_at IS NULL",
        )
        .bind(&identity.build_attempt_id)
        .bind(&identity.task_id)
        .bind(identity.delivery_generation)
        .bind(owner_incarnation_id)
        .execute(self.db.pool())
        .await?;
        Ok(result.rows_affected() > 0)
    }

    /// The C4 activation transaction.
    ///
    /// One transaction, one advisory lock, one compare-and-set. Every
    /// prerequisite is evaluated against rows read *inside* it, so a census that
    /// completes concurrently with a competing activation cannot both win.
    pub async fn activate(
        &self,
        input: &ActivateDirectDeliveryEpochInput,
    ) -> DbResult<ActivateDirectDeliveryEpochResult> {
        self.db.ensure_initialized().await?;
        require_nonblank("live_since", &input.live_since)?;
        if input.expected_generation < 0 {
            return Err(DbError::InvalidData(
                "expected_generation must not be negative".into(),
            ));
        }
        let target_generation = input.expected_generation.checked_add(1).ok_or_else(|| {
            DbError::InvalidData("expected_generation cannot be incremented".into())
        })?;

        let mut tx = self.db.pool().begin().await?;
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!(
                "direct-delivery-activation:{}",
                DirectDeliveryEpoch::NAME
            ))
            .execute(&mut *tx)
            .await?;

        // 1. Schema. Probed before any other statement so a new binary against
        //    an old schema refuses instead of erroring mid-transaction.
        let missing_relations = missing_relations(&mut tx).await?;
        if !missing_relations.is_empty() {
            tx.commit().await?;
            return Ok(ActivateDirectDeliveryEpochResult::Refused(
                DirectDeliveryActivationRefusal::MissingSchema { missing_relations },
            ));
        }

        // 2. The epoch row itself, locked for the rest of the transaction.
        let epoch: Option<(String, i64)> = sqlx::query_as(
            "SELECT state, generation FROM direct_delivery_epochs WHERE name = $1 FOR UPDATE",
        )
        .bind(DirectDeliveryEpoch::NAME)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((state, persisted_generation)) = epoch else {
            tx.commit().await?;
            return Ok(ActivateDirectDeliveryEpochResult::Refused(
                DirectDeliveryActivationRefusal::MissingEpoch,
            ));
        };
        let Ok(state) = state.parse::<DirectDeliveryEpochState>() else {
            tx.commit().await?;
            return Ok(ActivateDirectDeliveryEpochResult::Refused(
                DirectDeliveryActivationRefusal::UnknownEpochState {
                    state,
                    generation: persisted_generation,
                },
            ));
        };
        if state == DirectDeliveryEpochState::Active {
            tx.commit().await?;
            return Ok(if persisted_generation == target_generation {
                ActivateDirectDeliveryEpochResult::Replayed(DirectDeliveryEpoch::new(
                    state,
                    persisted_generation,
                )?)
            } else {
                ActivateDirectDeliveryEpochResult::Refused(
                    DirectDeliveryActivationRefusal::AlreadyActive {
                        generation: persisted_generation,
                    },
                )
            });
        }

        // 3. Generation CAS precondition: a competing activation that already
        //    moved the epoch, or a stale plan, refuses here.
        if persisted_generation != input.expected_generation {
            tx.commit().await?;
            return Ok(ActivateDirectDeliveryEpochResult::Refused(
                DirectDeliveryActivationRefusal::CompetingGeneration {
                    observed: input.expected_generation,
                    persisted: persisted_generation,
                },
            ));
        }

        // 4. Full live-process census at the target generation.
        let live_processes = live_process_ids(&mut tx, &input.live_since).await?;
        if live_processes.is_empty() {
            tx.commit().await?;
            return Ok(ActivateDirectDeliveryEpochResult::Refused(
                DirectDeliveryActivationRefusal::NoLiveProcesses,
            ));
        }
        let gaps = capability_census_gaps(&mut tx, target_generation, &input.live_since).await?;
        if !gaps.is_empty() {
            tx.commit().await?;
            return Ok(ActivateDirectDeliveryEpochResult::Refused(
                DirectDeliveryActivationRefusal::IncompleteCapabilityCensus { gaps },
            ));
        }

        // 5. Legacy-generation delivery leases must have drained or expired.
        let leases = live_legacy_delivery_leases(&mut tx, target_generation).await?;
        if !leases.is_empty() {
            tx.commit().await?;
            return Ok(ActivateDirectDeliveryEpochResult::Refused(
                DirectDeliveryActivationRefusal::LiveLegacyDeliveryLeases { leases },
            ));
        }

        // 6. The only non-test `state = 'active'` write in the tree.
        let activated: Option<(String, i64)> = sqlx::query_as(
            "UPDATE direct_delivery_epochs SET state = 'active', generation = $1, updated_at = now() \
             WHERE name = $2 AND state = 'disabled' AND generation = $3 \
             RETURNING state, generation",
        )
        .bind(target_generation)
        .bind(DirectDeliveryEpoch::NAME)
        .bind(input.expected_generation)
        .fetch_optional(&mut *tx)
        .await?;
        let Some((state, generation)) = activated else {
            tx.commit().await?;
            return Ok(ActivateDirectDeliveryEpochResult::Refused(
                DirectDeliveryActivationRefusal::CompetingGeneration {
                    observed: input.expected_generation,
                    persisted: persisted_generation,
                },
            ));
        };
        tx.commit().await?;
        let state = state
            .parse::<DirectDeliveryEpochState>()
            .map_err(DbError::InvalidData)?;
        Ok(ActivateDirectDeliveryEpochResult::Activated(
            DirectDeliveryEpoch::new(state, generation)?,
        ))
    }
}

async fn missing_relations(tx: &mut Transaction<'_, Postgres>) -> DbResult<Vec<String>> {
    let relations: Vec<Option<String>> = sqlx::query_scalar(
        "SELECT to_regclass('public.' || relation_name)::text \
         FROM unnest($1::text[]) AS relation_name",
    )
    .bind(REQUIRED_RELATIONS.as_slice())
    .fetch_all(&mut **tx)
    .await?;
    Ok(REQUIRED_RELATIONS
        .iter()
        .zip(relations)
        .filter_map(|(name, exists)| exists.is_none().then(|| (*name).to_owned()))
        .collect())
}

/// The census population: coordinator incarnations still renewing their lease.
/// This is deliberately the same liveness definition the orphan reaper uses, so
/// "live" cannot mean one thing to activation and another to recovery.
async fn live_process_ids(
    tx: &mut Transaction<'_, Postgres>,
    live_since: &str,
) -> DbResult<Vec<String>> {
    Ok(sqlx::query_scalar(
        "SELECT id FROM coordinator_incarnations WHERE last_renewed_at >= $1 ORDER BY id",
    )
    .bind(live_since)
    .fetch_all(&mut **tx)
    .await?)
}

async fn capability_census_gaps(
    tx: &mut Transaction<'_, Postgres>,
    target_generation: i64,
    live_since: &str,
) -> DbResult<Vec<CapabilityCensusGap>> {
    let rows: Vec<(String, Vec<String>)> = sqlx::query_as(
        "SELECT ci.id, \
                COALESCE(array_agg(c.capability) FILTER (WHERE c.capability IS NOT NULL), \
                         ARRAY[]::varchar[]) \
           FROM coordinator_incarnations ci \
           LEFT JOIN direct_delivery_process_capabilities c \
             ON c.process_incarnation_id = ci.id AND c.epoch_generation = $2 \
          WHERE ci.last_renewed_at >= $1 \
          GROUP BY ci.id \
          ORDER BY ci.id",
    )
    .bind(live_since)
    .bind(target_generation)
    .fetch_all(&mut **tx)
    .await?;

    let mut gaps = Vec::new();
    for (process_incarnation_id, advertised) in rows {
        let advertised: Vec<DirectDeliveryCapability> = advertised
            .iter()
            .filter_map(|value| DirectDeliveryCapability::from_str(value).ok())
            .collect();
        let missing: Vec<DirectDeliveryCapability> = DirectDeliveryCapability::ALL
            .into_iter()
            .filter(|capability| !advertised.contains(capability))
            .collect();
        if !missing.is_empty() {
            gaps.push(CapabilityCensusGap {
                process_incarnation_id,
                missing,
            });
        }
    }
    Ok(gaps)
}

async fn live_legacy_delivery_leases(
    tx: &mut Transaction<'_, Postgres>,
    target_generation: i64,
) -> DbResult<Vec<DirectDeliveryLease>> {
    let rows = sqlx::query_as::<_, LeaseRow>(&format!(
        "SELECT {LEASE_COLUMNS} FROM direct_delivery_leases \
         WHERE released_at IS NULL AND expires_at > now() AND epoch_generation < $1 \
         ORDER BY id"
    ))
    .bind(target_generation)
    .fetch_all(&mut **tx)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

async fn fetch_live_lease(
    tx: &mut Transaction<'_, Postgres>,
    identity: &TaskDeliveryIdentity,
) -> DbResult<Option<DirectDeliveryLease>> {
    let row = sqlx::query_as::<_, LeaseRow>(&format!(
        "SELECT {LEASE_COLUMNS} FROM direct_delivery_leases \
         WHERE build_attempt_id = $1 AND task_id = $2 AND delivery_generation = $3 \
           AND released_at IS NULL FOR UPDATE"
    ))
    .bind(&identity.build_attempt_id)
    .bind(&identity.task_id)
    .bind(identity.delivery_generation)
    .fetch_optional(&mut **tx)
    .await?;
    Ok(row.map(Into::into))
}

fn require_nonblank(field: &str, value: &str) -> DbResult<()> {
    if value.trim().is_empty() {
        return Err(DbError::InvalidData(format!("{field} must not be blank")));
    }
    Ok(())
}
