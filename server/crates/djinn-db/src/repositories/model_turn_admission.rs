//! Durable model-turn admission records, acquisition, and schema readiness.
//!
//! Admission serializes through Postgres; provider normalization and fenced
//! reconciliation remain owned by later phases.

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
    /// Capability discovery is durably assigned to exactly one request while
    /// the pool remains unknown. The owner must publish a capability result;
    /// non-owners retry after that state changes.
    DiscoveryRequired {
        owner_request_id: String,
        is_owner: bool,
    },
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

/// Result of a fenced lease mutation. `Fenced` never changes another lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelTurnLeaseMutationOutcome {
    Applied,
    Idempotent,
    Fenced,
}

/// The exact observation a watchdog must compare before expiring a lease.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnLeaseExpiryInput {
    pub identity: ModelTurnLeaseIdentity,
    pub observed_lifecycle: ModelTurnLeaseLifecycle,
    pub observed_heartbeat_at: Option<String>,
    pub boundary_at: String,
}

/// Provider usage which replaces the reservation estimate at reconciliation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnAuthoritativeUsage {
    pub request_units: i64,
    pub input_units: i64,
    pub output_units: i64,
    pub combined_units: i64,
}

/// One fenced terminal decision. Missing usage quarantines possibly-sent spend.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnLeaseReconciliationInput {
    pub identity: ModelTurnLeaseIdentity,
    pub outcome: ModelTurnLeaseTerminalOutcome,
    pub authoritative_usage: Option<ModelTurnAuthoritativeUsage>,
    pub detail: Option<String>,
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

/// Closed, redaction-safe reason code persisted with a Phase-C window.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnControllerWindowDiagnosticCode {
    EmptyExpectedDenominator,
    MissingCapability,
    UnexpectedCapability,
    DuplicateCapability,
    UncoveredCapability,
    PartialCapabilityCoverage,
    StaleHeartbeat,
    UnknownAttemptPath,
    MissingUsage,
    ExpiredLease,
    OpenBreaker,
    MissingStage,
    DuplicateStage,
    MissingStageOutcome,
    StageOutsideWindow,
    ReversedStages,
    InvalidStageOutcome,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ModelTurnControllerWindowDiagnostic {
    pub pool_id: i64,
    pub code: ModelTurnControllerWindowDiagnosticCode,
}
/// Labels are correlations only; the coordinator's live catalog owns authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelTurnControllerWindowSummary {
    pub provider_id: String,
    pub model_id: String,
    pub trainable: bool,
    pub diagnostics: Vec<ModelTurnControllerWindowDiagnostic>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelTurnControllerWindowInput {
    pub pool_id: i64,
    pub window_sequence: i64,
    pub started_at: String,
    pub ended_at: String,
    pub admitted_turns: i64,
    pub completed_turns: i64,
    pub summary: ModelTurnControllerWindowSummary,
}
/// Exact-bound DB projection. It deliberately contains no diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelTurnLearnerWindow {
    pub pool_id: i64,
    pub window_sequence: i64,
    pub started_at: String,
    pub ended_at: String,
    pub admitted_turns: i64,
    pub completed_turns: i64,
    pub provider_id: String,
    pub model_id: String,
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

/// A bounded, redaction-safe record written at the slot send boundary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnDecisionRecordInput {
    pub pool_id: i64,
    pub request_fingerprint: String,
    pub generation: i64,
    pub decision: ModelTurnDecisionKind,
    /// Closed diagnostic vocabulary; arbitrary diagnostic text is deliberately
    /// unrepresentable at this durable boundary.
    pub diagnostic: Option<ModelTurnDecisionDiagnostic>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnDecisionKind {
    ShadowPermit,
    EnforceAdmitted,
    Wait,
    Rejected,
}

/// Stable, non-identifying decision diagnostics persisted by their canonical
/// code. There is intentionally no string/catch-all variant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnDecisionDiagnostic {
    CapabilityUnknown,
    CapabilityUnsupported,
    PoolUnavailable,
    PolicyDraining,
    RequestInvalid,
}

impl ModelTurnDecisionDiagnostic {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::CapabilityUnknown => "capability_unknown",
            Self::CapabilityUnsupported => "capability_unsupported",
            Self::PoolUnavailable => "pool_unavailable",
            Self::PolicyDraining => "policy_draining",
            Self::RequestInvalid => "request_invalid",
        }
    }
}

/// Slot-bound, route-qualified report. It contains no request, lease, credential, or account identifier.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnCapabilityHeartbeatInput {
    pub pool_id: i64,
    pub slot_pod_uid: String,
    pub deployment_revision: String,
    pub provider_id: String,
    pub model_id: String,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnPhaseCEvidenceStage {
    Decision,
    Dispatch,
    Heartbeat,
    ProviderOutcome,
    Reconcile,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelTurnPhaseCEvidenceOutcome {
    Recorded,
    Succeeded,
    Failed,
    Missing,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTurnPhaseCEvidenceInput {
    pub pool_id: i64,
    pub slot_pod_uid: String,
    pub deployment_revision: String,
    pub provider_id: String,
    pub model_id: String,
    pub attempt_fingerprint: String,
    pub stage: ModelTurnPhaseCEvidenceStage,
    pub outcome: ModelTurnPhaseCEvidenceOutcome,
}

/// A bounded attempt-chain edge with the timestamp needed for an aligned,
/// half-open controller window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, sqlx::FromRow)]
pub struct ModelTurnPhaseCEvidence {
    pub slot_pod_uid: String,
    pub deployment_revision: String,
    pub provider_id: String,
    pub model_id: String,
    pub attempt_fingerprint: String,
    pub stage: String,
    pub outcome: String,
    pub recorded_at: String,
}

/// Durable repository surface for the additive v1 schema.
#[derive(Clone)]
pub struct ModelTurnAdmissionRepository {
    db: Database,
}

#[derive(sqlx::FromRow)]
struct ModelTurnPoolRow {
    id: i64,
    credential_id: String,
    provider_id: String,
    model_id: String,
    phase: String,
    identity_state: String,
    capability_state: String,
    learned_concurrency: i64,
    in_flight: i64,
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

    /// Resolve an existing Phase A pool without creating a second ledger.
    pub async fn resolve_pool(
        &self,
        credential_id: &str,
        provider_id: &str,
        model_id: &str,
    ) -> Result<Option<ModelTurnPool>> {
        self.db.ensure_initialized().await?;
        let row: Option<ModelTurnPoolRow> = sqlx::query_as(
            "SELECT id, credential_id, provider_id, model_id, phase, identity_state, capability_state, learned_concurrency, in_flight FROM model_turn_pools WHERE credential_id = $1 AND provider_id = $2 AND model_id = $3",
        ).bind(credential_id).bind(provider_id).bind(model_id).fetch_optional(self.db.pool()).await?;
        row.map(|row| {
            Ok(ModelTurnPool {
                id: row.id,
                credential_id: row.credential_id,
                provider_id: row.provider_id,
                model_id: row.model_id,
                phase: parse_phase(&row.phase)?,
                identity_state: parse_identity(&row.identity_state)?,
                capability_state: parse_capability(&row.capability_state)?,
                learned_concurrency: row.learned_concurrency,
                in_flight: row.in_flight,
            })
        })
        .transpose()
    }

    /// Resolve an existing pool through B1's opaque credential-record scope.
    /// The match is exact and unique, so callers cannot pick another credential
    /// merely because it shares the provider/model labels.
    pub async fn resolve_pool_by_credential_fingerprint(
        &self,
        credential_fingerprint: &str,
        provider_id: &str,
        model_id: &str,
    ) -> Result<Option<ModelTurnPool>> {
        self.db.ensure_initialized().await?;
        let rows: Vec<ModelTurnPoolRow> = sqlx::query_as(
            "SELECT id, credential_id, provider_id, model_id, phase, identity_state, capability_state, learned_concurrency, in_flight FROM model_turn_pools WHERE provider_id = $1 AND model_id = $2 ORDER BY id",
        )
        .bind(provider_id)
        .bind(model_id)
        .fetch_all(self.db.pool())
        .await?;
        let mut matches = rows.into_iter().filter(|row| {
            use sha2::Digest;
            format!(
                "sha256:{:x}",
                sha2::Sha256::digest(row.credential_id.as_bytes())
            ) == credential_fingerprint
        });
        let Some(row) = matches.next() else {
            return Ok(None);
        };
        if matches.next().is_some() {
            return Err(crate::Error::InvalidData(
                "ambiguous model-turn credential route".to_owned(),
            ));
        }
        Ok(Some(ModelTurnPool {
            id: row.id,
            credential_id: row.credential_id,
            provider_id: row.provider_id,
            model_id: row.model_id,
            phase: parse_phase(&row.phase)?,
            identity_state: parse_identity(&row.identity_state)?,
            capability_state: parse_capability(&row.capability_state)?,
            learned_concurrency: row.learned_concurrency,
            in_flight: row.in_flight,
        }))
    }

    /// Persist a decision before returning a shadow send permit. Inputs contain
    /// only a one-way request fingerprint and bounded diagnostic vocabulary.
    pub async fn record_decision(&self, input: ModelTurnDecisionRecordInput) -> Result<()> {
        self.db.ensure_initialized().await?;
        if input.pool_id <= 0
            || input.generation <= 0
            || !is_sha256_fingerprint(&input.request_fingerprint)
        {
            return Err(crate::Error::InvalidData(
                "invalid model-turn decision record".to_owned(),
            ));
        }
        sqlx::query("INSERT INTO model_turn_decisions (pool_id, request_fingerprint, generation, decision, diagnostic) VALUES ($1, $2, $3, $4, $5) ON CONFLICT (pool_id, request_fingerprint, generation) DO NOTHING")
            .bind(input.pool_id).bind(input.request_fingerprint).bind(input.generation)
            .bind(decision_kind_name(input.decision))
            .bind(input.diagnostic.map(ModelTurnDecisionDiagnostic::code))
            .execute(self.db.pool()).await?;
        Ok(())
    }

    /// Persist only a live slot identity after deriving route labels from its admitted pool.
    pub async fn record_capability_heartbeat(
        &self,
        input: ModelTurnCapabilityHeartbeatInput,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        if !valid_phase_c_identity(
            input.pool_id,
            &input.slot_pod_uid,
            &input.deployment_revision,
            &input.provider_id,
            &input.model_id,
        ) {
            return invalid_phase_c();
        }
        let mut tx = self.db.pool().begin().await?;
        let result = sqlx::query("INSERT INTO model_turn_capability_heartbeats (pool_id, slot_pod_uid, deployment_revision, provider_id, model_id) SELECT id, $2, $3, provider_id, model_id FROM model_turn_pools WHERE id = $1 AND provider_id = $4 AND model_id = $5 ON CONFLICT (pool_id, slot_pod_uid, deployment_revision) DO UPDATE SET heartbeat_at = now(), provider_id = EXCLUDED.provider_id, model_id = EXCLUDED.model_id")
            .bind(input.pool_id).bind(&input.slot_pod_uid).bind(&input.deployment_revision).bind(&input.provider_id).bind(&input.model_id).execute(&mut *tx).await?;
        if result.rows_affected() != 1 {
            return invalid_phase_c();
        }
        tx.commit().await?;
        Ok(())
    }

    /// Read the bounded, route-qualified heartbeat projection for coverage windows.
    pub async fn recent_capability_heartbeats(
        &self,
        pool_id: i64,
        limit: i64,
    ) -> Result<Vec<(String, String, String, String, String)>> {
        self.db.ensure_initialized().await?;
        sqlx::query_as("SELECT slot_pod_uid, deployment_revision, provider_id, model_id, heartbeat_at::text FROM model_turn_capability_heartbeats WHERE pool_id = $1 ORDER BY heartbeat_at DESC LIMIT $2")
            .bind(pool_id).bind(limit.clamp(1, 256)).fetch_all(self.db.pool()).await.map_err(Into::into)
    }

    /// Persist a closed-vocabulary attempt-chain edge only for the exact admitted route.
    pub async fn record_phase_c_evidence(&self, input: ModelTurnPhaseCEvidenceInput) -> Result<()> {
        self.db.ensure_initialized().await?;
        if !valid_phase_c_identity(
            input.pool_id,
            &input.slot_pod_uid,
            &input.deployment_revision,
            &input.provider_id,
            &input.model_id,
        ) || !is_sha256_fingerprint(&input.attempt_fingerprint)
        {
            return invalid_phase_c();
        }
        let mut tx = self.db.pool().begin().await?;
        let result = sqlx::query("INSERT INTO model_turn_phase_c_evidence (pool_id, slot_pod_uid, deployment_revision, provider_id, model_id, attempt_fingerprint, stage, outcome) SELECT id, $2, $3, provider_id, model_id, $6, $7, $8 FROM model_turn_pools WHERE id = $1 AND provider_id = $4 AND model_id = $5")
            .bind(input.pool_id).bind(&input.slot_pod_uid).bind(&input.deployment_revision).bind(&input.provider_id).bind(&input.model_id).bind(&input.attempt_fingerprint).bind(phase_c_stage_name(input.stage)).bind(phase_c_outcome_name(input.outcome)).execute(&mut *tx).await?;
        if result.rows_affected() != 1 {
            return invalid_phase_c();
        }
        tx.commit().await?;
        Ok(())
    }

    /// Read recent bounded attempt evidence, including its persistence timestamp.
    pub async fn recent_phase_c_evidence(
        &self,
        pool_id: i64,
        limit: i64,
    ) -> Result<Vec<ModelTurnPhaseCEvidence>> {
        self.db.ensure_initialized().await?;
        sqlx::query_as("SELECT slot_pod_uid, deployment_revision, provider_id, model_id, attempt_fingerprint, stage, outcome, recorded_at::text AS recorded_at FROM model_turn_phase_c_evidence WHERE pool_id = $1 ORDER BY recorded_at DESC, id DESC LIMIT $2")
            .bind(pool_id).bind(limit.clamp(1, 256)).fetch_all(self.db.pool()).await.map_err(Into::into)
    }

    /// Read bounded evidence inside `[start_at, end_at)` for an aligned window.
    pub async fn phase_c_evidence_in_window(
        &self,
        pool_id: i64,
        start_at: &str,
        end_at: &str,
        limit: i64,
    ) -> Result<Vec<ModelTurnPhaseCEvidence>> {
        self.db.ensure_initialized().await?;
        if pool_id <= 0 || start_at.trim().is_empty() || end_at.trim().is_empty() {
            return invalid_phase_c();
        }
        sqlx::query_as("SELECT slot_pod_uid, deployment_revision, provider_id, model_id, attempt_fingerprint, stage, outcome, recorded_at::text AS recorded_at FROM model_turn_phase_c_evidence WHERE pool_id = $1 AND recorded_at >= $2::timestamptz AND recorded_at < $3::timestamptz AND $2::timestamptz < $3::timestamptz ORDER BY recorded_at ASC, id ASC LIMIT $4")
            .bind(pool_id).bind(start_at).bind(end_at).bind(limit.clamp(1, 256)).fetch_all(self.db.pool()).await.map_err(Into::into)
    }

    /// Typed bounded storage; production catalog qualification belongs to coordinator.
    pub async fn upsert_controller_window(
        &self,
        input: ModelTurnControllerWindowInput,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        validate_controller_window_input(&input)?;
        let summary = serde_json::to_string(&input.summary)
            .map_err(|e| crate::Error::InvalidData(e.to_string()))?;
        let changed = sqlx::query("INSERT INTO model_turn_controller_windows (pool_id, window_sequence, started_at, ended_at, admitted_turns, completed_turns, summary) SELECT id, $2, $3::timestamptz, $4::timestamptz, $5, $6, $7 FROM model_turn_pools WHERE id = $1 ON CONFLICT (pool_id, window_sequence) DO UPDATE SET started_at = EXCLUDED.started_at, ended_at = EXCLUDED.ended_at, admitted_turns = EXCLUDED.admitted_turns, completed_turns = EXCLUDED.completed_turns, summary = EXCLUDED.summary")
            .bind(input.pool_id).bind(input.window_sequence).bind(&input.started_at).bind(&input.ended_at).bind(input.admitted_turns).bind(input.completed_turns).bind(summary).execute(self.db.pool()).await?;
        if changed.rows_affected() != 1 {
            return invalid_phase_c();
        }
        Ok(())
    }

    /// Read one persisted typed summary for cross-crate storage regressions.
    ///
    /// This test-support seam keeps raw SQL inside `djinn-db`; production
    /// learners must continue through the exact-bound fail-closed projection.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn controller_window_summary_for_test(
        &self,
        pool_id: i64,
        window_sequence: i64,
    ) -> Result<Option<ModelTurnControllerWindowSummary>> {
        self.db.ensure_initialized().await?;
        let summary: Option<String> = sqlx::query_scalar(
            "SELECT summary FROM model_turn_controller_windows WHERE pool_id = $1 AND window_sequence = $2",
        )
        .bind(pool_id)
        .bind(window_sequence)
        .fetch_optional(self.db.pool())
        .await?;
        summary
            .map(|summary| {
                serde_json::from_str(&summary)
                    .map_err(|error| crate::Error::InvalidData(error.to_string()))
            })
            .transpose()
    }

    /// Overwrite the durable pool label pair for a fail-closed learner
    /// regression. Raw mutation stays in DB test support so dependent crates do
    /// not acquire SQL access solely to model damaged rows.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn set_pool_labels_for_test(
        &self,
        pool_id: i64,
        provider_id: &str,
        model_id: &str,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("UPDATE model_turn_pools SET provider_id = $2, model_id = $3 WHERE id = $1")
            .bind(pool_id)
            .bind(provider_id)
            .bind(model_id)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Write a controller-window row verbatim, bypassing the typed write
    /// validator, so fail-closed read regressions can model rows that a
    /// corrupted or downlevel writer could leave behind. Raw mutation stays in
    /// DB test support so dependent crates do not acquire SQL access solely to
    /// model damaged rows.
    #[cfg(any(test, feature = "test-support"))]
    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_raw_controller_window_for_test(
        &self,
        pool_id: i64,
        window_sequence: i64,
        started_at: &str,
        ended_at: &str,
        admitted_turns: i64,
        completed_turns: i64,
        summary: &str,
    ) -> Result<()> {
        self.db.ensure_initialized().await?;
        sqlx::query("INSERT INTO model_turn_controller_windows (pool_id, window_sequence, started_at, ended_at, admitted_turns, completed_turns, summary) VALUES ($1, $2, $3::timestamptz, $4::timestamptz, $5, $6, $7) ON CONFLICT (pool_id, window_sequence) DO UPDATE SET started_at = EXCLUDED.started_at, ended_at = EXCLUDED.ended_at, admitted_turns = EXCLUDED.admitted_turns, completed_turns = EXCLUDED.completed_turns, summary = EXCLUDED.summary")
            .bind(pool_id)
            .bind(window_sequence)
            .bind(started_at)
            .bind(ended_at)
            .bind(admitted_turns)
            .bind(completed_turns)
            .bind(summary)
            .execute(self.db.pool())
            .await?;
        Ok(())
    }

    /// Exact-bound fail-closed projection; the coordinator revalidates catalog membership.
    pub async fn learner_window(
        &self,
        pool_id: i64,
        window_sequence: i64,
        started_at: &str,
        ended_at: &str,
    ) -> Result<Option<ModelTurnLearnerWindow>> {
        self.db.ensure_initialized().await?;
        if pool_id <= 0 || window_sequence < 0 || !valid_aligned_minute_bounds(started_at, ended_at)
        {
            return Ok(None);
        }
        // `timestamptz::text` renders `1970-01-01 00:02:00+00`, which is not
        // RFC 3339. Render an explicitly UTC, microsecond-precision RFC 3339
        // string so the read-side alignment check sees the real stored instant
        // instead of failing every row on a formatting artifact.
        let row: Option<ControllerWindowRow> = sqlx::query_as("SELECT w.pool_id,w.window_sequence,to_char(w.started_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'),to_char(w.ended_at AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS.US\"Z\"'),w.admitted_turns,w.completed_turns,w.summary,p.provider_id,p.model_id FROM model_turn_controller_windows w JOIN model_turn_pools p ON p.id=w.pool_id WHERE w.pool_id=$1 AND w.window_sequence=$2 AND w.started_at=$3::timestamptz AND w.ended_at=$4::timestamptz").bind(pool_id).bind(window_sequence).bind(started_at).bind(ended_at).fetch_optional(self.db.pool()).await?;
        Ok(row.and_then(project_learner_window))
    }

    /// Commit the pre-send fence before a caller sends provider network bytes.
    pub async fn mark_dispatching(
        &self,
        identity: &ModelTurnLeaseIdentity,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.transition(identity, "reserved", "dispatching", true)
            .await
    }

    /// Move the same fenced lease from dispatching to active.
    pub async fn mark_active(
        &self,
        identity: &ModelTurnLeaseIdentity,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.transition(identity, "dispatching", "active", false)
            .await
    }

    /// Persist a heartbeat only for the identity which still owns an in-flight lease.
    pub async fn heartbeat(
        &self,
        identity: &ModelTurnLeaseIdentity,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.db.ensure_initialized().await?;
        let changed = sqlx::query("UPDATE model_turn_leases SET heartbeat_at = now() WHERE lease_id = $1::uuid AND generation = $2 AND request_id = $3 AND lifecycle IN ('dispatching', 'active')")
            .bind(&identity.lease_id).bind(identity.generation).bind(&identity.request_id).execute(self.db.pool()).await?;
        Ok(if changed.rows_affected() == 1 {
            ModelTurnLeaseMutationOutcome::Applied
        } else {
            ModelTurnLeaseMutationOutcome::Fenced
        })
    }

    /// Compare-and-swap only the watchdog's stale observation after 90 seconds.
    pub async fn expire_lease(
        &self,
        input: ModelTurnLeaseExpiryInput,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.db.ensure_initialized().await?;
        // A watchdog may only terminalize an in-flight observation. Never let
        // a replay present a terminal state as its expected state: terminal
        // accounting already happened, so a second insert is neither safe nor
        // an idempotent expiry operation.
        if !is_in_flight_lifecycle(input.observed_lifecycle) {
            return Ok(ModelTurnLeaseMutationOutcome::Fenced);
        }
        let mut tx = self.db.pool().begin().await?;
        let lease: Option<(i64, String, String)> = sqlx::query_as("SELECT pool_id, reservation_id::text, lifecycle FROM model_turn_leases WHERE lease_id = $1::uuid AND generation = $2 AND request_id = $3 AND lifecycle = $4 AND lifecycle IN ('reserved', 'dispatching', 'active') AND heartbeat_at IS NOT DISTINCT FROM $5::timestamptz AND COALESCE(heartbeat_at, reserved_at) <= $6::timestamptz - interval '90 seconds' FOR UPDATE")
            .bind(&input.identity.lease_id).bind(input.identity.generation).bind(&input.identity.request_id).bind(lease_lifecycle_name(input.observed_lifecycle)).bind(&input.observed_heartbeat_at).bind(&input.boundary_at).fetch_optional(&mut *tx).await?;
        let Some((pool_id, reservation_id, lifecycle)) = lease else {
            return Ok(ModelTurnLeaseMutationOutcome::Fenced);
        };
        sqlx::query("UPDATE model_turn_leases SET lifecycle = 'expired', terminal_at = $2::timestamptz WHERE lease_id = $1::uuid AND generation = $3 AND request_id = $4")
            .bind(&input.identity.lease_id).bind(&input.boundary_at).bind(input.identity.generation).bind(&input.identity.request_id).execute(&mut *tx).await?;
        let unsent = lifecycle == "reserved";
        sqlx::query("INSERT INTO model_turn_lease_terminals (lease_id, generation, request_id, outcome, accounting_state) VALUES ($1::uuid, $2, $3, 'expired', $4)")
            .bind(&input.identity.lease_id).bind(input.identity.generation).bind(&input.identity.request_id).bind(if unsent { "refunded" } else { "quarantined" }).execute(&mut *tx).await?;
        sqlx::query("UPDATE model_turn_reservations SET state = 'expired', terminal_at = $2::timestamptz WHERE id = $1::uuid").bind(&reservation_id).bind(&input.boundary_at).execute(&mut *tx).await?;
        self.release_accounting(&mut tx, pool_id, &reservation_id, unsent, None)
            .await?;
        tx.commit().await?;
        Ok(ModelTurnLeaseMutationOutcome::Applied)
    }

    /// Alias for the sole terminal reconciliation/accounting path.
    pub async fn reconcile(
        &self,
        input: ModelTurnLeaseReconciliationInput,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.reconcile_lease(input).await
    }

    /// Alias for callers which model expiry as the terminal action itself.
    pub async fn expire(
        &self,
        input: ModelTurnLeaseExpiryInput,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.expire_lease(input).await
    }

    /// Record one terminal outcome and apply reservation accounting at most once.
    pub async fn reconcile_lease(
        &self,
        input: ModelTurnLeaseReconciliationInput,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.reconcile_lease_with_unsent_dispatching(input, false)
            .await
    }

    /// Cancel an attempt which has not been handed to provider I/O. The slot
    /// fence may already be `dispatching`; that state alone must not quarantine
    /// a permit which was dropped before it could be sent.
    pub async fn cancel_before_send(
        &self,
        identity: ModelTurnLeaseIdentity,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.reconcile_lease_with_unsent_dispatching(
            ModelTurnLeaseReconciliationInput {
                identity,
                outcome: ModelTurnLeaseTerminalOutcome::Cancelled,
                authoritative_usage: None,
                detail: None,
            },
            true,
        )
        .await
    }

    async fn reconcile_lease_with_unsent_dispatching(
        &self,
        input: ModelTurnLeaseReconciliationInput,
        dispatching_is_definitely_unsent: bool,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.db.ensure_initialized().await?;
        if input.detail.as_ref().is_some_and(|v| v.len() > 1024)
            || input
                .authoritative_usage
                .as_ref()
                .is_some_and(usage_is_negative)
        {
            return Err(crate::Error::InvalidData(
                "invalid model-turn reconciliation".to_owned(),
            ));
        }
        let mut tx = self.db.pool().begin().await?;
        let lease: Option<(i64, String, String)> = sqlx::query_as("SELECT pool_id, reservation_id::text, lifecycle FROM model_turn_leases WHERE lease_id = $1::uuid AND generation = $2 AND request_id = $3 FOR UPDATE").bind(&input.identity.lease_id).bind(input.identity.generation).bind(&input.identity.request_id).fetch_optional(&mut *tx).await?;
        let Some((pool_id, reservation_id, lifecycle)) = lease else {
            return Ok(ModelTurnLeaseMutationOutcome::Fenced);
        };
        let terminal: Option<(String, String)> = sqlx::query_as("SELECT outcome, accounting_state FROM model_turn_lease_terminals WHERE lease_id = $1::uuid AND generation = $2 AND request_id = $3 FOR UPDATE").bind(&input.identity.lease_id).bind(input.identity.generation).bind(&input.identity.request_id).fetch_optional(&mut *tx).await?;
        if let Some((existing, accounting_state)) = terminal {
            if existing != terminal_outcome_name(input.outcome) {
                return Ok(ModelTurnLeaseMutationOutcome::Fenced);
            }
            if let Some(usage) = input.authoritative_usage.as_ref()
                && accounting_state == "quarantined"
            {
                self.resolve_quarantined_usage(&mut tx, pool_id, &reservation_id, usage)
                    .await?;
                sqlx::query("UPDATE model_turn_lease_terminals SET accounting_state = 'authoritative' WHERE lease_id = $1::uuid AND generation = $2 AND request_id = $3 AND accounting_state = 'quarantined'")
                    .bind(&input.identity.lease_id).bind(input.identity.generation).bind(&input.identity.request_id).execute(&mut *tx).await?;
                tx.commit().await?;
                return Ok(ModelTurnLeaseMutationOutcome::Applied);
            }
            return Ok(ModelTurnLeaseMutationOutcome::Idempotent);
        }
        if !matches!(lifecycle.as_str(), "reserved" | "dispatching" | "active") {
            return Ok(ModelTurnLeaseMutationOutcome::Fenced);
        }
        let unsent = lifecycle == "reserved"
            || (dispatching_is_definitely_unsent && lifecycle == "dispatching");
        let accounting_state = if unsent {
            "refunded"
        } else if input.authoritative_usage.is_some() {
            "authoritative"
        } else {
            "quarantined"
        };
        sqlx::query("INSERT INTO model_turn_lease_terminals (lease_id, generation, request_id, outcome, detail, accounting_state) VALUES ($1::uuid, $2, $3, $4, $5, $6)").bind(&input.identity.lease_id).bind(input.identity.generation).bind(&input.identity.request_id).bind(terminal_outcome_name(input.outcome)).bind(&input.detail).bind(accounting_state).execute(&mut *tx).await?;
        sqlx::query("UPDATE model_turn_leases SET lifecycle = 'reconciled', terminal_at = now() WHERE lease_id = $1::uuid AND generation = $2 AND request_id = $3").bind(&input.identity.lease_id).bind(input.identity.generation).bind(&input.identity.request_id).execute(&mut *tx).await?;
        let state = if input.outcome == ModelTurnLeaseTerminalOutcome::Cancelled {
            "cancelled"
        } else {
            "reconciled"
        };
        sqlx::query("UPDATE model_turn_reservations SET state = $2, terminal_at = now() WHERE id = $1::uuid").bind(&reservation_id).bind(state).execute(&mut *tx).await?;
        self.release_accounting(
            &mut tx,
            pool_id,
            &reservation_id,
            unsent,
            input.authoritative_usage.as_ref(),
        )
        .await?;
        tx.commit().await?;
        Ok(ModelTurnLeaseMutationOutcome::Applied)
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
                // The pool row is locked above, so this insert elects one
                // durable owner across independent processes/connections.
                // Discovery completion changes `capability_state`; until then
                // all non-owners have an explicit durable retry condition.
                let owner_request_id: String = sqlx::query_scalar(
                    "INSERT INTO model_turn_capability_discoveries \
                     (pool_id, owner_request_id, owner_pod_uid) \
                     VALUES ($1, $2, $3) \
                     ON CONFLICT (pool_id) DO UPDATE \
                     SET owner_request_id = model_turn_capability_discoveries.owner_request_id \
                     RETURNING owner_request_id",
                )
                .bind(input.pool_id)
                .bind(&input.request_id)
                .bind(&input.owner_pod_uid)
                .fetch_one(&mut *tx)
                .await?;
                let outcome =
                    ModelTurnAcquireOutcome::Wait(ModelTurnAdmissionWait::DiscoveryRequired {
                        is_owner: owner_request_id == input.request_id,
                        owner_request_id,
                    });
                // Unlike the other wait/rejection paths, electing discovery
                // ownership mutates durable cross-process coordination. Commit
                // it before reporting the owner so another connection cannot
                // subsequently elect itself after this transaction drops.
                tx.commit().await?;
                return Ok(outcome);
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

    async fn transition(
        &self,
        identity: &ModelTurnLeaseIdentity,
        from: &str,
        to: &str,
        dispatch: bool,
    ) -> Result<ModelTurnLeaseMutationOutcome> {
        self.db.ensure_initialized().await?;
        let mut tx = self.db.pool().begin().await?;
        let changed = sqlx::query("UPDATE model_turn_leases SET lifecycle = $4, dispatching_at = CASE WHEN $5 THEN now() ELSE dispatching_at END, active_at = CASE WHEN NOT $5 THEN now() ELSE active_at END WHERE lease_id = $1::uuid AND generation = $2 AND request_id = $3 AND lifecycle = $6")
            .bind(&identity.lease_id).bind(identity.generation).bind(&identity.request_id).bind(to).bind(dispatch).bind(from).execute(&mut *tx).await?;
        if changed.rows_affected() == 1 {
            if dispatch {
                sqlx::query("UPDATE model_turn_reservations SET state = 'dispatched' WHERE id = (SELECT reservation_id FROM model_turn_leases WHERE lease_id = $1::uuid)").bind(&identity.lease_id).execute(&mut *tx).await?;
            }
            tx.commit().await?;
            return Ok(ModelTurnLeaseMutationOutcome::Applied);
        }
        let lifecycle: Option<String> = sqlx::query_scalar("SELECT lifecycle FROM model_turn_leases WHERE lease_id = $1::uuid AND generation = $2 AND request_id = $3")
            .bind(&identity.lease_id).bind(identity.generation).bind(&identity.request_id).fetch_optional(&mut *tx).await?;
        Ok(if lifecycle.as_deref() == Some(to) {
            ModelTurnLeaseMutationOutcome::Idempotent
        } else {
            ModelTurnLeaseMutationOutcome::Fenced
        })
    }

    async fn release_accounting(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        pool_id: i64,
        reservation_id: &str,
        unsent: bool,
        usage: Option<&ModelTurnAuthoritativeUsage>,
    ) -> Result<()> {
        sqlx::query("UPDATE model_turn_pools SET in_flight = GREATEST(0, in_flight - 1), updated_at = now() WHERE id = $1").bind(pool_id).execute(&mut **tx).await?;
        let buckets: Vec<(String, i64)> = sqlx::query_as("SELECT bucket_kind, reserved_units FROM model_turn_reservation_buckets WHERE reservation_id = $1::uuid ORDER BY bucket_kind FOR UPDATE").bind(reservation_id).fetch_all(&mut **tx).await?;
        for (kind, reserved) in buckets {
            if unsent {
                sqlx::query("UPDATE model_turn_bucket_bindings SET available_units = LEAST(capacity_units, available_units + $3), updated_at = now() WHERE pool_id = $1 AND bucket_kind = $2").bind(pool_id).bind(&kind).bind(reserved).execute(&mut **tx).await?;
            } else if let Some(usage) = usage {
                sqlx::query("UPDATE model_turn_bucket_bindings SET available_units = LEAST(capacity_units, GREATEST(0, available_units + $3 - $4)), updated_at = now() WHERE pool_id = $1 AND bucket_kind = $2").bind(pool_id).bind(&kind).bind(reserved).bind(usage_for_kind(usage, &kind)).execute(&mut **tx).await?;
            } else {
                sqlx::query("UPDATE model_turn_bucket_bindings SET quarantined_units = quarantined_units + $3, updated_at = now() WHERE pool_id = $1 AND bucket_kind = $2").bind(pool_id).bind(&kind).bind(reserved).execute(&mut **tx).await?;
            }
        }
        Ok(())
    }

    /// Replace a previously quarantined reservation debit exactly once. The
    /// terminal row is locked by the caller, so a replay cannot credit twice.
    async fn resolve_quarantined_usage(
        &self,
        tx: &mut Transaction<'_, Postgres>,
        pool_id: i64,
        reservation_id: &str,
        usage: &ModelTurnAuthoritativeUsage,
    ) -> Result<()> {
        let buckets: Vec<(String, i64)> = sqlx::query_as("SELECT bucket_kind, reserved_units FROM model_turn_reservation_buckets WHERE reservation_id = $1::uuid ORDER BY bucket_kind FOR UPDATE")
            .bind(reservation_id).fetch_all(&mut **tx).await?;
        for (kind, reserved) in buckets {
            sqlx::query("UPDATE model_turn_bucket_bindings SET available_units = LEAST(capacity_units, GREATEST(0, available_units + $3 - $4)), quarantined_units = GREATEST(0, quarantined_units - $3), updated_at = now() WHERE pool_id = $1 AND bucket_kind = $2")
                .bind(pool_id).bind(&kind).bind(reserved).bind(usage_for_kind(usage, &kind)).execute(&mut **tx).await?;
        }
        Ok(())
    }
}

fn lease_lifecycle_name(lifecycle: ModelTurnLeaseLifecycle) -> &'static str {
    match lifecycle {
        ModelTurnLeaseLifecycle::Reserved => "reserved",
        ModelTurnLeaseLifecycle::Dispatching => "dispatching",
        ModelTurnLeaseLifecycle::Active => "active",
        ModelTurnLeaseLifecycle::Reconciled => "reconciled",
        ModelTurnLeaseLifecycle::Expired => "expired",
    }
}

fn is_in_flight_lifecycle(lifecycle: ModelTurnLeaseLifecycle) -> bool {
    matches!(
        lifecycle,
        ModelTurnLeaseLifecycle::Reserved
            | ModelTurnLeaseLifecycle::Dispatching
            | ModelTurnLeaseLifecycle::Active
    )
}

fn terminal_outcome_name(outcome: ModelTurnLeaseTerminalOutcome) -> &'static str {
    match outcome {
        ModelTurnLeaseTerminalOutcome::Completed => "completed",
        ModelTurnLeaseTerminalOutcome::Cancelled => "cancelled",
        ModelTurnLeaseTerminalOutcome::Expired => "expired",
        ModelTurnLeaseTerminalOutcome::Failed => "failed",
    }
}

fn usage_is_negative(usage: &ModelTurnAuthoritativeUsage) -> bool {
    usage.request_units < 0
        || usage.input_units < 0
        || usage.output_units < 0
        || usage.combined_units < 0
}

fn usage_for_kind(usage: &ModelTurnAuthoritativeUsage, kind: &str) -> i64 {
    match kind {
        "request" => usage.request_units,
        "input" => usage.input_units,
        "output" => usage.output_units,
        "combined" => usage.combined_units,
        _ => 0,
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

fn decision_kind_name(kind: ModelTurnDecisionKind) -> &'static str {
    match kind {
        ModelTurnDecisionKind::ShadowPermit => "shadow_permit",
        ModelTurnDecisionKind::EnforceAdmitted => "enforce_admitted",
        ModelTurnDecisionKind::Wait => "wait",
        ModelTurnDecisionKind::Rejected => "rejected",
    }
}
fn is_sha256_fingerprint(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value.as_bytes()[7..]
            .iter()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}
/// Widest durable summary the `model_turn_controller_windows.summary` column
/// accepts. Rejecting oversize summaries in Rust keeps a bounded-diagnostic
/// window from failing as an opaque Postgres `22001` string-truncation error.
const CONTROLLER_WINDOW_SUMMARY_MAX_BYTES: usize = 2048;

/// Raw projection tuple of the exact-bound controller-window read.
type ControllerWindowRow = (i64, i64, String, String, i64, i64, String, String, String);

/// Canonical RFC 3339 rendering of an exact aligned 60-second half-open window,
/// or `None` when the pair is not such a window.
fn canonical_aligned_minute_bounds(started_at: &str, ended_at: &str) -> Option<(String, String)> {
    let start = chrono::DateTime::parse_from_rfc3339(started_at).ok()?;
    let end = chrono::DateTime::parse_from_rfc3339(ended_at).ok()?;
    if start.timestamp_subsec_nanos() != 0
        || end.timestamp_subsec_nanos() != 0
        || start.timestamp().rem_euclid(60) != 0
        || end.timestamp() != start.timestamp() + 60
    {
        return None;
    }
    Some((
        start.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        end.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    ))
}
fn valid_aligned_minute_bounds(started_at: &str, ended_at: &str) -> bool {
    canonical_aligned_minute_bounds(started_at, ended_at).is_some()
}

/// Fail-closed read-side projection. Every rejection path returns `None`, so a
/// damaged, diagnostic, or downlevel durable row is simply invisible to the
/// learner rather than surfacing as a partially trusted window.
fn project_learner_window(row: ControllerWindowRow) -> Option<ModelTurnLearnerWindow> {
    let (
        pool_id,
        window_sequence,
        started_at,
        ended_at,
        admitted_turns,
        completed_turns,
        summary,
        pool_provider,
        pool_model,
    ) = row;
    if pool_id <= 0 || window_sequence < 0 || admitted_turns < 0 || completed_turns < 0 {
        return None;
    }
    let (started_at, ended_at) = canonical_aligned_minute_bounds(&started_at, &ended_at)?;
    if !chrono::DateTime::parse_from_rfc3339(&started_at)
        .is_ok_and(|start| start.timestamp().div_euclid(60) == window_sequence)
    {
        return None;
    }
    if summary.len() > CONTROLLER_WINDOW_SUMMARY_MAX_BYTES {
        return None;
    }
    let summary = serde_json::from_str::<ModelTurnControllerWindowSummary>(&summary).ok()?;
    if !summary.trainable
        || !summary.diagnostics.is_empty()
        || summary.provider_id != pool_provider
        || summary.model_id != pool_model
    {
        return None;
    }
    Some(ModelTurnLearnerWindow {
        pool_id,
        window_sequence,
        started_at,
        ended_at,
        admitted_turns,
        completed_turns,
        provider_id: summary.provider_id,
        model_id: summary.model_id,
    })
}
fn validate_controller_window_input(input: &ModelTurnControllerWindowInput) -> Result<()> {
    let oversize = serde_json::to_string(&input.summary)
        .map(|summary| summary.len() > CONTROLLER_WINDOW_SUMMARY_MAX_BYTES)
        .unwrap_or(true);
    if input.pool_id <= 0
        || input.window_sequence < 0
        || input.admitted_turns < 0
        || input.completed_turns < 0
        || !valid_aligned_minute_bounds(&input.started_at, &input.ended_at)
        || !chrono::DateTime::parse_from_rfc3339(&input.started_at)
            .is_ok_and(|s| s.timestamp().div_euclid(60) == input.window_sequence)
        || input.summary.provider_id.trim().is_empty()
        || input.summary.model_id.trim().is_empty()
        || input.summary.provider_id.len() > 191
        || input.summary.model_id.len() > 191
        || (input.summary.trainable && !input.summary.diagnostics.is_empty())
        || input.summary.diagnostics.len() > 64
        || input
            .summary
            .diagnostics
            .iter()
            .any(|d| d.pool_id != 0 && d.pool_id != input.pool_id)
        || oversize
    {
        return Err(crate::Error::InvalidData(
            "invalid model-turn controller window".to_owned(),
        ));
    }
    Ok(())
}
fn valid_phase_c_identity(
    pool_id: i64,
    slot: &str,
    revision: &str,
    provider: &str,
    model: &str,
) -> bool {
    pool_id > 0
        && !slot.trim().is_empty()
        && slot.len() <= 255
        && !revision.trim().is_empty()
        && revision.len() <= 255
        && !provider.trim().is_empty()
        && provider.len() <= 191
        && !model.trim().is_empty()
        && model.len() <= 191
}
fn invalid_phase_c<T>() -> Result<T> {
    Err(crate::Error::InvalidData(
        "invalid Phase-C evidence".to_owned(),
    ))
}
fn phase_c_stage_name(stage: ModelTurnPhaseCEvidenceStage) -> &'static str {
    match stage {
        ModelTurnPhaseCEvidenceStage::Decision => "decision",
        ModelTurnPhaseCEvidenceStage::Dispatch => "dispatch",
        ModelTurnPhaseCEvidenceStage::Heartbeat => "heartbeat",
        ModelTurnPhaseCEvidenceStage::ProviderOutcome => "provider_outcome",
        ModelTurnPhaseCEvidenceStage::Reconcile => "reconcile",
    }
}
fn phase_c_outcome_name(outcome: ModelTurnPhaseCEvidenceOutcome) -> &'static str {
    match outcome {
        ModelTurnPhaseCEvidenceOutcome::Recorded => "recorded",
        ModelTurnPhaseCEvidenceOutcome::Succeeded => "succeeded",
        ModelTurnPhaseCEvidenceOutcome::Failed => "failed",
        ModelTurnPhaseCEvidenceOutcome::Missing => "missing",
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

    #[test]
    fn fingerprints_match_the_lowercase_sql_check() {
        let valid = format!("sha256:{}", "a".repeat(64));
        assert!(is_sha256_fingerprint(&valid));
        assert!(!is_sha256_fingerprint(&valid.to_uppercase()));
        assert!(!is_sha256_fingerprint("sha256:abc"));
    }

    #[test]
    fn controller_window_summary_serialization_is_closed_and_pool_local() {
        let summary = ModelTurnControllerWindowSummary {
            provider_id: "provider".into(),
            model_id: "namespace/model".into(),
            trainable: false,
            diagnostics: vec![ModelTurnControllerWindowDiagnostic {
                pool_id: 7,
                code: ModelTurnControllerWindowDiagnosticCode::MissingCapability,
            }],
        };
        assert_eq!(
            serde_json::to_value(&summary).expect("serialize closed summary"),
            serde_json::json!({
                "provider_id": "provider", "model_id": "namespace/model",
                "trainable": false,
                "diagnostics": [{"pool_id": 7, "code": "missing_capability"}],
            })
        );
        assert!(
            serde_json::from_value::<ModelTurnControllerWindowSummary>(serde_json::json!({
                "provider_id": "provider", "model_id": "model", "trainable": false,
                "diagnostics": [], "reporter_text": "forbidden"
            }))
            .is_err()
        );
        let input = ModelTurnControllerWindowInput {
            pool_id: 7,
            window_sequence: 2,
            started_at: "1970-01-01T00:02:00Z".into(),
            ended_at: "1970-01-01T00:03:00Z".into(),
            admitted_turns: 0,
            completed_turns: 0,
            summary,
        };
        assert!(validate_controller_window_input(&input).is_ok());
        let unrelated = ModelTurnControllerWindowInput {
            summary: ModelTurnControllerWindowSummary {
                diagnostics: vec![ModelTurnControllerWindowDiagnostic {
                    pool_id: 8,
                    code: ModelTurnControllerWindowDiagnosticCode::MissingCapability,
                }],
                ..input.summary.clone()
            },
            ..input
        };
        assert!(validate_controller_window_input(&unrelated).is_err());
    }

    #[test]
    fn controller_window_write_rejects_every_out_of_contract_dimension() {
        let valid = ModelTurnControllerWindowInput {
            pool_id: 7,
            window_sequence: 2,
            started_at: "1970-01-01T00:02:00Z".into(),
            ended_at: "1970-01-01T00:03:00Z".into(),
            admitted_turns: 3,
            completed_turns: 3,
            summary: ModelTurnControllerWindowSummary {
                provider_id: "provider".into(),
                model_id: "model".into(),
                trainable: true,
                diagnostics: Vec::new(),
            },
        };
        assert!(validate_controller_window_input(&valid).is_ok());

        let mutate = |f: &dyn Fn(&mut ModelTurnControllerWindowInput)| {
            let mut input = valid.clone();
            f(&mut input);
            input
        };
        let rejected: Vec<(&str, ModelTurnControllerWindowInput)> = vec![
            ("nonpositive pool", mutate(&|i| i.pool_id = 0)),
            ("negative pool", mutate(&|i| i.pool_id = -1)),
            (
                "negative sequence",
                mutate(&|i| {
                    i.window_sequence = -1;
                }),
            ),
            (
                "negative admitted count",
                mutate(&|i| i.admitted_turns = -1),
            ),
            (
                "negative completed count",
                mutate(&|i| i.completed_turns = -1),
            ),
            (
                "subsecond start",
                mutate(&|i| {
                    i.started_at = "1970-01-01T00:02:00.5Z".into();
                }),
            ),
            (
                "unaligned start",
                mutate(&|i| {
                    i.started_at = "1970-01-01T00:02:30Z".into();
                    i.ended_at = "1970-01-01T00:03:30Z".into();
                }),
            ),
            (
                "ninety second span",
                mutate(&|i| i.ended_at = "1970-01-01T00:03:30Z".into()),
            ),
            (
                "thirty second span",
                mutate(&|i| i.ended_at = "1970-01-01T00:02:30Z".into()),
            ),
            (
                "reversed bounds",
                mutate(&|i| i.ended_at = "1970-01-01T00:01:00Z".into()),
            ),
            (
                "unparsable bounds",
                mutate(&|i| i.started_at = "not-a-time".into()),
            ),
            (
                "sequence disagrees with start",
                mutate(&|i| i.window_sequence = 3),
            ),
            (
                "blank provider",
                mutate(&|i| i.summary.provider_id = "   ".into()),
            ),
            (
                "blank model",
                mutate(&|i| i.summary.model_id = String::new()),
            ),
            (
                "overlong provider",
                mutate(&|i| i.summary.provider_id = "p".repeat(192)),
            ),
            (
                "overlong model",
                mutate(&|i| i.summary.model_id = "m".repeat(192)),
            ),
            (
                "trainable with diagnostics",
                mutate(&|i| {
                    i.summary.diagnostics = vec![ModelTurnControllerWindowDiagnostic {
                        pool_id: 7,
                        code: ModelTurnControllerWindowDiagnosticCode::MissingUsage,
                    }];
                }),
            ),
            (
                "unbounded diagnostics",
                mutate(&|i| {
                    i.summary.trainable = false;
                    i.summary.diagnostics = (0..65)
                        .map(|_| ModelTurnControllerWindowDiagnostic {
                            pool_id: 7,
                            code: ModelTurnControllerWindowDiagnosticCode::MissingUsage,
                        })
                        .collect();
                }),
            ),
            (
                "foreign pool diagnostic",
                mutate(&|i| {
                    i.summary.trainable = false;
                    i.summary.diagnostics = vec![ModelTurnControllerWindowDiagnostic {
                        pool_id: 8,
                        code: ModelTurnControllerWindowDiagnosticCode::MissingUsage,
                    }];
                }),
            ),
            (
                "summary wider than the durable column",
                mutate(&|i| {
                    i.summary.trainable = false;
                    i.summary.diagnostics = (0..64)
                        .map(|_| ModelTurnControllerWindowDiagnostic {
                            pool_id: 7,
                            code:
                                ModelTurnControllerWindowDiagnosticCode::PartialCapabilityCoverage,
                        })
                        .collect();
                }),
            ),
        ];
        for (label, input) in rejected {
            assert!(
                validate_controller_window_input(&input).is_err(),
                "{label} must be rejected by the typed storage boundary"
            );
        }
        // Every accepted summary fits the durable column, so a bounded
        // diagnostic window can never fail as a Postgres truncation error.
        let bounded = mutate(&|i| {
            i.summary.trainable = false;
            i.summary.diagnostics = (0..30)
                .map(|_| ModelTurnControllerWindowDiagnostic {
                    pool_id: 7,
                    code: ModelTurnControllerWindowDiagnosticCode::PartialCapabilityCoverage,
                })
                .collect();
        });
        assert!(validate_controller_window_input(&bounded).is_ok());
        assert!(
            serde_json::to_string(&bounded.summary)
                .expect("serialize")
                .len()
                <= CONTROLLER_WINDOW_SUMMARY_MAX_BYTES
        );
    }

    #[test]
    fn learner_projection_fails_closed_on_every_damaged_durable_row() {
        let trainable = serde_json::to_string(&ModelTurnControllerWindowSummary {
            provider_id: "provider".into(),
            model_id: "model".into(),
            trainable: true,
            diagnostics: Vec::new(),
        })
        .expect("serialize trainable summary");
        let row = |summary: &str| -> ControllerWindowRow {
            (
                7,
                2,
                "1970-01-01T00:02:00.000000Z".into(),
                "1970-01-01T00:03:00.000000Z".into(),
                3,
                3,
                summary.to_owned(),
                "provider".into(),
                "model".into(),
            )
        };
        let accepted = project_learner_window(row(&trainable)).expect("canonical row projects");
        assert_eq!(accepted.started_at, "1970-01-01T00:02:00Z");
        assert_eq!(accepted.ended_at, "1970-01-01T00:03:00Z");
        assert_eq!(accepted.admitted_turns, 3);

        let mutate = |f: &dyn Fn(&mut ControllerWindowRow)| {
            let mut row = row(&trainable);
            f(&mut row);
            row
        };
        let rejected: Vec<(&str, ControllerWindowRow)> = vec![
            ("nonpositive pool", mutate(&|r| r.0 = 0)),
            ("negative sequence", mutate(&|r| r.1 = -1)),
            ("negative admitted count", mutate(&|r| r.4 = -1)),
            ("negative completed count", mutate(&|r| r.5 = -1)),
            (
                "subsecond start",
                mutate(&|r| r.2 = "1970-01-01T00:02:00.000001Z".into()),
            ),
            (
                "unaligned start",
                mutate(&|r| {
                    r.2 = "1970-01-01T00:02:30.000000Z".into();
                    r.3 = "1970-01-01T00:03:30.000000Z".into();
                }),
            ),
            (
                "ninety second span",
                mutate(&|r| r.3 = "1970-01-01T00:03:30.000000Z".into()),
            ),
            (
                "sequence disagrees with start",
                mutate(&|r| {
                    r.2 = "1970-01-01T00:03:00.000000Z".into();
                    r.3 = "1970-01-01T00:04:00.000000Z".into();
                }),
            ),
            ("unparsable bounds", mutate(&|r| r.2 = "not-a-time".into())),
            ("malformed summary json", mutate(&|r| r.6 = "{".into())),
            (
                "summary with an extra key",
                mutate(&|r| {
                    r.6 = r#"{"provider_id":"provider","model_id":"model","trainable":true,"diagnostics":[],"reporter_text":"leak"}"#.into();
                }),
            ),
            (
                "summary with an unknown reason code",
                mutate(&|r| {
                    r.6 = r#"{"provider_id":"provider","model_id":"model","trainable":false,"diagnostics":[{"pool_id":7,"code":"free_text"}]}"#.into();
                }),
            ),
            (
                "summary wider than the durable column",
                mutate(&|r| r.6 = "x".repeat(CONTROLLER_WINDOW_SUMMARY_MAX_BYTES + 1)),
            ),
            (
                "not trainable",
                mutate(&|r| {
                    r.6 = r#"{"provider_id":"provider","model_id":"model","trainable":false,"diagnostics":[]}"#.into();
                }),
            ),
            (
                "diagnostic window",
                mutate(&|r| {
                    r.6 = r#"{"provider_id":"provider","model_id":"model","trainable":true,"diagnostics":[{"pool_id":7,"code":"missing_usage"}]}"#.into();
                }),
            ),
            ("provider label mismatch", mutate(&|r| r.7 = "other".into())),
            ("model label mismatch", mutate(&|r| r.8 = "other".into())),
        ];
        for (label, row) in rejected {
            assert!(
                project_learner_window(row).is_none(),
                "{label} must yield no learner window"
            );
        }
    }

    #[test]
    fn decision_diagnostics_have_only_fixed_non_identifying_codes() {
        let codes = [
            ModelTurnDecisionDiagnostic::CapabilityUnknown.code(),
            ModelTurnDecisionDiagnostic::CapabilityUnsupported.code(),
            ModelTurnDecisionDiagnostic::PoolUnavailable.code(),
            ModelTurnDecisionDiagnostic::PolicyDraining.code(),
            ModelTurnDecisionDiagnostic::RequestInvalid.code(),
        ];
        assert!(codes.iter().all(|code| {
            code.len() <= 32
                && code
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
        }));
    }
}
