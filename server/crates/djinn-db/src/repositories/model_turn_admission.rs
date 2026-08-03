//! Inert v1 records and schema readiness for model-turn admission.
//!
//! This module deliberately has no acquisition or lifecycle mutation methods.
//! Later phases own serializable admission, provider normalization, and fenced
//! reconciliation; Phase A only establishes stable typed storage vocabulary.

use serde::{Deserialize, Serialize};

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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

/// Read-only repository surface for validating the additive v1 schema.
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
