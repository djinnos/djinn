//! Inert v1 records and schema readiness for model-turn admission.
//!
//! This module deliberately has no acquisition or lifecycle mutation methods.
//! Later phases own serializable admission, provider normalization, and fenced
//! reconciliation; Phase A only establishes stable typed storage vocabulary.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};

use crate::{Database, Result};

/// The durable model-turn admission schema revision understood by this binary.
pub const MODEL_TURN_ADMISSION_SCHEMA_VERSION: i64 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnAdmissionPhase {
    Off,
    Shadow,
    Draining,
    Enforce,
}

/// A requested debit of a persisted admission bucket.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnBucketDebit {
    pub bucket_kind: ModelTurnBucketKind,
    pub units: i64,
}

/// Input to the atomic model-turn acquisition operation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnAcquireInput {
    pub pool_id: i64,
    pub request_id: String,
    pub owner_pod_uid: Option<String>,
    /// Generation is copied once into the new lease and is never updated.
    pub generation: i64,
    pub debits: Vec<ModelTurnBucketDebit>,
}

pub type ModelTurnAcquireTurnInput = ModelTurnAcquireInput;

/// A durable condition which may change and permits a later retry.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelTurnAdmissionWait {
    Concurrency {
        target: i64,
        in_flight: i64,
    },
    ResetAt {
        bucket_kind: ModelTurnBucketKind,
        reset_at: String,
    },
    DiscoveryRequired,
    Draining,
    BindingUnavailable {
        bucket_kind: ModelTurnBucketKind,
    },
    BucketUnavailable {
        bucket_kind: ModelTurnBucketKind,
        available_units: i64,
        required_units: i64,
        reset_at: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelTurnAdmissionRejection {
    PoolUnavailable,
    Off,
    ShadowOnly,
    IneligibleIdentity { state: ModelTurnIdentityState },
    UnsupportedCapability { state: ModelTurnCapabilityState },
    InvalidRequest,
    RequestConflict,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelTurnAcquireOutcome {
    Admitted {
        reservation: ModelTurnReservation,
        lease: ModelTurnLease,
        idempotent: bool,
    },
    Wait(ModelTurnAdmissionWait),
    Rejected(ModelTurnAdmissionRejection),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnIdentityState {
    Eligible,
    Revoked,
    Ambiguous,
    Colliding,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnCapabilityState {
    Unknown,
    Supported,
    Unsupported,
    Degraded,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnBucketKind {
    Request,
    Input,
    Output,
    Combined,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnReservationState {
    Reserved,
    Dispatched,
    Reconciled,
    Expired,
    Cancelled,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnLeaseLifecycle {
    Reserved,
    Dispatching,
    Active,
    Reconciled,
    Expired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnLeaseTerminalOutcome {
    Completed,
    Cancelled,
    Expired,
    Failed,
}

/// A pool is keyed exclusively by the durable credential row and provider/model
/// scope. It intentionally carries neither credential material nor user IDs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnPool {
    pub id: i64,
    pub credential_id: String,
    pub provider_id: String,
    pub model_id: String,
    pub phase: ModelTurnAdmissionPhase,
    pub identity_state: ModelTurnIdentityState,
    pub capability_state: ModelTurnCapabilityState,
    pub learned_concurrency: i64,
    pub in_flight: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnBucketBinding {
    pub pool_id: i64,
    pub bucket_kind: ModelTurnBucketKind,
    pub capacity_units: i64,
    pub available_units: i64,
    pub authoritative_epoch: i64,
    pub quarantined_units: i64,
    pub observation_sequence: i64,
    pub reset_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnReservation {
    pub id: String,
    pub pool_id: i64,
    pub request_id: String,
    pub state: ModelTurnReservationState,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnReservationBucket {
    pub reservation_id: String,
    pub pool_id: i64,
    pub bucket_kind: ModelTurnBucketKind,
    pub reserved_units: i64,
}

/// A random lease id paired with its immutable generation and stable request
/// identity. `new` is the only constructor in Phase A and does not persist.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnLeaseIdentity {
    pub lease_id: String,
    pub generation: i64,
    pub request_id: String,
}

impl ModelTurnLeaseIdentity {
    #[must_use]
    pub fn new(generation: i64, request_id: impl Into<String>) -> Self {
        Self {
            lease_id: uuid::Uuid::new_v4().to_string(),
            generation,
            request_id: request_id.into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnLease {
    pub identity: ModelTurnLeaseIdentity,
    pub pool_id: i64,
    pub reservation_id: String,
    pub owner_pod_uid: Option<String>,
    pub lifecycle: ModelTurnLeaseLifecycle,
    pub heartbeat_at: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnControllerWindow {
    pub pool_id: i64,
    pub window_sequence: i64,
    pub admitted_turns: i64,
    pub completed_turns: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnObservation {
    pub pool_id: i64,
    pub sequence: i64,
    pub kind: String,
    pub request_units: i64,
    pub input_units: i64,
    pub output_units: i64,
    /// A bounded, non-secret diagnostic string.
    pub detail: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnAdmissionSchemaReadiness {
    pub model_turn_admission_schema: i64,
}

/// Durable repository surface for the additive v1 schema.
#[derive(Clone)]
pub struct ModelTurnAdmissionRepository {
    db: Database,
}

impl ModelTurnAdmissionRepository {
    #[must_use]
    pub fn new(db: Database) -> Self {
        Self { db }
    }

    /// Report the installed schema marker. This does not activate admission.
    pub async fn schema_readiness(&self) -> Result<Option<ModelTurnAdmissionSchemaReadiness>> {
        self.db.ensure_initialized().await?;
        let version: Option<i64> = sqlx::query_scalar(
            "SELECT version FROM model_turn_admission_schema \
             WHERE marker = 'model_turn_admission_schema'",
        )
        .fetch_optional(self.db.pool())
        .await?;
        Ok(version.map(
            |model_turn_admission_schema| ModelTurnAdmissionSchemaReadiness {
                model_turn_admission_schema,
            },
        ))
    }

    /// Atomically acquire learned concurrency and every supplied bucket debit.
    ///
    /// Each attempt is SERIALIZABLE and locks in one documented order: the pool
    /// first, then distinct binding rows ordered by persisted `bucket_kind`.
    /// Retrying a serialization abort reruns the whole decision; no local lock
    /// or partially-applied debit is ever used as admission authority.
    pub async fn acquire_turn(
        &self,
        input: ModelTurnAcquireInput,
    ) -> Result<ModelTurnAcquireOutcome> {
        self.db.ensure_initialized().await?;
        if input.pool_id <= 0
            || input.request_id.trim().is_empty()
            || input.request_id.len() > 128
            || input.generation <= 0
        {
            return Ok(ModelTurnAcquireOutcome::Rejected(
                ModelTurnAdmissionRejection::InvalidRequest,
            ));
        }
        for attempt in 0..3 {
            match self.acquire_turn_once(&input).await {
                Err(error) if attempt < 2 && is_serialization_failure(&error) => continue,
                result => return result,
            }
        }
        unreachable!("the bounded retry loop returns on its final iteration")
    }

    async fn acquire_turn_once(
        &self,
        input: &ModelTurnAcquireInput,
    ) -> Result<ModelTurnAcquireOutcome> {
        let debits = canonical_debits(&input.debits)?;
        let mut tx = self.db.pool().begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
            .execute(&mut *tx)
            .await?;
        let pool: Option<(String, String, String, i64, i64)> = sqlx::query_as(
            "SELECT phase, identity_state, capability_state, learned_concurrency, in_flight FROM model_turn_pools WHERE id = $1 FOR UPDATE",
        ).bind(input.pool_id).fetch_optional(&mut *tx).await?;
        let Some((phase, identity_state, capability_state, target, in_flight)) = pool else {
            return commit_outcome(ModelTurnAcquireOutcome::Rejected(
                ModelTurnAdmissionRejection::PoolUnavailable,
            ));
        };
        let kinds: Vec<String> = debits
            .keys()
            .map(|kind| bucket_kind_name(*kind).to_owned())
            .collect();
        let rows: Vec<(String, i64, Option<String>)> = sqlx::query_as(
            "SELECT bucket_kind, available_units, reset_at::text FROM model_turn_bucket_bindings WHERE pool_id = $1 AND bucket_kind = ANY($2::text[]) ORDER BY bucket_kind FOR UPDATE",
        ).bind(input.pool_id).bind(&kinds).fetch_all(&mut *tx).await?;
        let bindings: BTreeMap<_, _> = rows
            .into_iter()
            .map(|(kind, units, reset)| (kind, (units, reset)))
            .collect();

        // Lookup follows the two admission lock classes, so an identical replay
        // cannot create a second lease or debit while a rival is in flight.
        if let Some(outcome) = existing_reservation_outcome(&mut tx, input, &debits).await? {
            return commit_outcome(outcome);
        }
        match parse_phase(&phase)? {
            ModelTurnAdmissionPhase::Off => {
                return commit_outcome(ModelTurnAcquireOutcome::Rejected(
                    ModelTurnAdmissionRejection::Off,
                ));
            }
            ModelTurnAdmissionPhase::Shadow => {
                return commit_outcome(ModelTurnAcquireOutcome::Rejected(
                    ModelTurnAdmissionRejection::ShadowOnly,
                ));
            }
            ModelTurnAdmissionPhase::Draining => {
                return commit_outcome(ModelTurnAcquireOutcome::Wait(
                    ModelTurnAdmissionWait::Draining,
                ));
            }
            ModelTurnAdmissionPhase::Enforce => {}
        }
        let identity_state = parse_identity(&identity_state)?;
        if identity_state != ModelTurnIdentityState::Eligible {
            return commit_outcome(ModelTurnAcquireOutcome::Rejected(
                ModelTurnAdmissionRejection::IneligibleIdentity {
                    state: identity_state,
                },
            ));
        }
        match parse_capability(&capability_state)? {
            ModelTurnCapabilityState::Unknown => {
                return commit_outcome(ModelTurnAcquireOutcome::Wait(
                    ModelTurnAdmissionWait::DiscoveryRequired,
                ));
            }
            state
            @ (ModelTurnCapabilityState::Unsupported | ModelTurnCapabilityState::Degraded) => {
                return commit_outcome(ModelTurnAcquireOutcome::Rejected(
                    ModelTurnAdmissionRejection::UnsupportedCapability { state },
                ));
            }
            ModelTurnCapabilityState::Supported => {}
        }
        if target <= in_flight {
            return commit_outcome(ModelTurnAcquireOutcome::Wait(
                ModelTurnAdmissionWait::Concurrency { target, in_flight },
            ));
        }
        for (kind, required) in &debits {
            let Some((available, reset_at)) = bindings.get(bucket_kind_name(*kind)) else {
                return commit_outcome(ModelTurnAcquireOutcome::Wait(
                    ModelTurnAdmissionWait::BindingUnavailable { bucket_kind: *kind },
                ));
            };
            if let Some(reset_at) = reset_at {
                return commit_outcome(ModelTurnAcquireOutcome::Wait(
                    ModelTurnAdmissionWait::ResetAt {
                        bucket_kind: *kind,
                        reset_at: reset_at.clone(),
                    },
                ));
            }
            if available < required {
                return commit_outcome(ModelTurnAcquireOutcome::Wait(
                    ModelTurnAdmissionWait::BucketUnavailable {
                        bucket_kind: *kind,
                        available_units: *available,
                        required_units: *required,
                        reset_at: reset_at.clone(),
                    },
                ));
            }
        }
        let reservation_id = uuid::Uuid::new_v4().to_string();
        let lease_identity =
            ModelTurnLeaseIdentity::new(input.generation, input.request_id.clone());
        sqlx::query("UPDATE model_turn_pools SET in_flight = in_flight + 1, updated_at = now() WHERE id = $1").bind(input.pool_id).execute(&mut *tx).await?;
        for (kind, units) in &debits {
            sqlx::query("UPDATE model_turn_bucket_bindings SET available_units = available_units - $3, updated_at = now() WHERE pool_id = $1 AND bucket_kind = $2")
                .bind(input.pool_id).bind(bucket_kind_name(*kind)).bind(*units).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO model_turn_reservations (id, pool_id, request_id) VALUES ($1::uuid, $2, $3)").bind(&reservation_id).bind(input.pool_id).bind(&input.request_id).execute(&mut *tx).await?;
        for (kind, units) in &debits {
            sqlx::query("INSERT INTO model_turn_reservation_buckets (reservation_id, pool_id, bucket_kind, reserved_units) VALUES ($1::uuid, $2, $3, $4)")
                .bind(&reservation_id).bind(input.pool_id).bind(bucket_kind_name(*kind)).bind(*units).execute(&mut *tx).await?;
        }
        sqlx::query("INSERT INTO model_turn_leases (lease_id, generation, pool_id, reservation_id, request_id, owner_pod_uid) VALUES ($1::uuid, $2, $3, $4::uuid, $5, $6)")
            .bind(&lease_identity.lease_id).bind(lease_identity.generation).bind(input.pool_id).bind(&reservation_id).bind(&input.request_id).bind(&input.owner_pod_uid).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(ModelTurnAcquireOutcome::Admitted {
            reservation: ModelTurnReservation {
                id: reservation_id.clone(),
                pool_id: input.pool_id,
                request_id: input.request_id.clone(),
                state: ModelTurnReservationState::Reserved,
            },
            lease: ModelTurnLease {
                identity: lease_identity,
                pool_id: input.pool_id,
                reservation_id,
                owner_pod_uid: input.owner_pod_uid.clone(),
                lifecycle: ModelTurnLeaseLifecycle::Reserved,
                heartbeat_at: None,
            },
            idempotent: false,
        })
    }
}

fn bucket_kind_name(kind: ModelTurnBucketKind) -> &'static str {
    match kind {
        ModelTurnBucketKind::Request => "request",
        ModelTurnBucketKind::Input => "input",
        ModelTurnBucketKind::Output => "output",
        ModelTurnBucketKind::Combined => "combined",
    }
}

fn canonical_debits(debits: &[ModelTurnBucketDebit]) -> Result<BTreeMap<ModelTurnBucketKind, i64>> {
    let mut result = BTreeMap::new();
    for debit in debits {
        if debit.units < 0 {
            return Err(crate::Error::InvalidData(
                "model-turn bucket debit is negative".to_owned(),
            ));
        }
        let units = result.entry(debit.bucket_kind).or_insert(0_i64);
        *units = units.checked_add(debit.units).ok_or_else(|| {
            crate::Error::InvalidData("model-turn bucket debit overflows".to_owned())
        })?;
    }
    Ok(result)
}

fn commit_outcome(outcome: ModelTurnAcquireOutcome) -> Result<ModelTurnAcquireOutcome> {
    Ok(outcome)
}

fn parse_phase(value: &str) -> Result<ModelTurnAdmissionPhase> {
    match value {
        "off" => Ok(ModelTurnAdmissionPhase::Off),
        "shadow" => Ok(ModelTurnAdmissionPhase::Shadow),
        "draining" => Ok(ModelTurnAdmissionPhase::Draining),
        "enforce" => Ok(ModelTurnAdmissionPhase::Enforce),
        _ => Err(crate::Error::InvalidData(format!(
            "invalid model-turn phase: {value}"
        ))),
    }
}
fn parse_identity(value: &str) -> Result<ModelTurnIdentityState> {
    match value {
        "eligible" => Ok(ModelTurnIdentityState::Eligible),
        "revoked" => Ok(ModelTurnIdentityState::Revoked),
        "ambiguous" => Ok(ModelTurnIdentityState::Ambiguous),
        "colliding" => Ok(ModelTurnIdentityState::Colliding),
        _ => Err(crate::Error::InvalidData(format!(
            "invalid model-turn identity state: {value}"
        ))),
    }
}
fn parse_capability(value: &str) -> Result<ModelTurnCapabilityState> {
    match value {
        "unknown" => Ok(ModelTurnCapabilityState::Unknown),
        "supported" => Ok(ModelTurnCapabilityState::Supported),
        "unsupported" => Ok(ModelTurnCapabilityState::Unsupported),
        "degraded" => Ok(ModelTurnCapabilityState::Degraded),
        _ => Err(crate::Error::InvalidData(format!(
            "invalid model-turn capability state: {value}"
        ))),
    }
}
fn parse_reservation_state(value: &str) -> Result<ModelTurnReservationState> {
    match value {
        "reserved" => Ok(ModelTurnReservationState::Reserved),
        "dispatched" => Ok(ModelTurnReservationState::Dispatched),
        "reconciled" => Ok(ModelTurnReservationState::Reconciled),
        "expired" => Ok(ModelTurnReservationState::Expired),
        "cancelled" => Ok(ModelTurnReservationState::Cancelled),
        _ => Err(crate::Error::InvalidData(format!(
            "invalid reservation state: {value}"
        ))),
    }
}
fn parse_lease_lifecycle(value: &str) -> Result<ModelTurnLeaseLifecycle> {
    match value {
        "reserved" => Ok(ModelTurnLeaseLifecycle::Reserved),
        "dispatching" => Ok(ModelTurnLeaseLifecycle::Dispatching),
        "active" => Ok(ModelTurnLeaseLifecycle::Active),
        "reconciled" => Ok(ModelTurnLeaseLifecycle::Reconciled),
        "expired" => Ok(ModelTurnLeaseLifecycle::Expired),
        _ => Err(crate::Error::InvalidData(format!(
            "invalid lease lifecycle: {value}"
        ))),
    }
}

async fn existing_reservation_outcome(
    tx: &mut Transaction<'_, Postgres>,
    input: &ModelTurnAcquireInput,
    debits: &BTreeMap<ModelTurnBucketKind, i64>,
) -> Result<Option<ModelTurnAcquireOutcome>> {
    let existing: Option<(String, String, String, i64, Option<String>, String)> = sqlx::query_as("SELECT r.id::text, r.state, l.lease_id::text, l.generation, l.owner_pod_uid, l.lifecycle FROM model_turn_reservations r JOIN model_turn_leases l ON l.reservation_id = r.id WHERE r.pool_id = $1 AND r.request_id = $2 FOR UPDATE OF r, l").bind(input.pool_id).bind(&input.request_id).fetch_optional(&mut **tx).await?;
    let Some((reservation_id, reservation_state, lease_id, generation, owner_pod_uid, lifecycle)) =
        existing
    else {
        return Ok(None);
    };
    let persisted: Vec<(String, i64)> = sqlx::query_as("SELECT bucket_kind, reserved_units FROM model_turn_reservation_buckets WHERE reservation_id = $1::uuid ORDER BY bucket_kind FOR UPDATE").bind(&reservation_id).fetch_all(&mut **tx).await?;
    let expected: Vec<_> = debits
        .iter()
        .map(|(kind, units)| (bucket_kind_name(*kind).to_owned(), *units))
        .collect();
    if persisted != expected
        || generation != input.generation
        || owner_pod_uid != input.owner_pod_uid
    {
        return Ok(Some(ModelTurnAcquireOutcome::Rejected(
            ModelTurnAdmissionRejection::RequestConflict,
        )));
    }
    Ok(Some(ModelTurnAcquireOutcome::Admitted {
        reservation: ModelTurnReservation {
            id: reservation_id.clone(),
            pool_id: input.pool_id,
            request_id: input.request_id.clone(),
            state: parse_reservation_state(&reservation_state)?,
        },
        lease: ModelTurnLease {
            identity: ModelTurnLeaseIdentity {
                lease_id,
                generation,
                request_id: input.request_id.clone(),
            },
            pool_id: input.pool_id,
            reservation_id,
            owner_pod_uid,
            lifecycle: parse_lease_lifecycle(&lifecycle)?,
            heartbeat_at: None,
        },
        idempotent: true,
    }))
}

fn is_serialization_failure(error: &crate::Error) -> bool {
    matches!(error, crate::Error::Sqlx(sqlx::Error::Database(database_error)) if matches!(database_error.code().as_deref(), Some("40001" | "40P01")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_identity_uses_a_random_id_and_retains_inputs() {
        let lease = ModelTurnLeaseIdentity::new(7, "request-1");
        assert!(uuid::Uuid::parse_str(&lease.lease_id).is_ok());
        assert_eq!(lease.generation, 7);
        assert_eq!(lease.request_id, "request-1");
    }

    #[test]
    fn schema_version_is_v1() {
        assert_eq!(MODEL_TURN_ADMISSION_SCHEMA_VERSION, 1);
    }
}
