//! Slot-boundary admission. Provider I/O remains outside this coordinator.
//!
//! `prepare` is a send fence: it returns an enforced permit only after the
//! exact Phase A lease's dispatch transition has committed.

use std::{collections::BTreeMap, sync::Arc};

use djinn_db::{
    ModelTurnAcquireInput, ModelTurnAcquireOutcome, ModelTurnAdmissionPhase,
    ModelTurnAdmissionRejection, ModelTurnAdmissionRepository, ModelTurnAdmissionWait,
    ModelTurnDecisionKind, ModelTurnDecisionRecordInput, ModelTurnLeaseIdentity,
    ModelTurnLeaseMutationOutcome, ModelTurnLeaseReconciliationInput,
    ModelTurnLeaseTerminalOutcome,
};
use djinn_provider::{ProviderAttemptPlanV1, ProviderAttemptTerminalV1, ProviderOutcomeV1};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

#[derive(Clone, Debug)]
pub struct ModelTurnAdmissionRequest {
    pub credential_id: String,
    pub request_id: String,
    pub owner_pod_uid: Option<String>,
    pub generation: i64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModelTurnSendPermit {
    pub lease: Option<ModelTurnLeaseIdentity>,
}

/// Phase A's typed outcomes are deliberately retained at the slot boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ModelTurnPreparation {
    Permit(ModelTurnSendPermit),
    Wait(ModelTurnAdmissionWait),
    Rejected(ModelTurnAdmissionRejection),
    /// Acquisition succeeded, but the pre-send fence did not. The identity is
    /// retained by this coordinator for cancellation/reconciliation ownership.
    DispatchFenced {
        identity: ModelTurnLeaseIdentity,
        outcome: ModelTurnLeaseMutationOutcome,
    },
}

/// Holds acquired identities only between acquisition and permit hand-off. This
/// is not a ledger: Phase A remains the accounting authority. It makes a future
/// cancellation during `mark_dispatching` recoverable via `cancel_pending`.
#[derive(Clone)]
pub struct ModelTurnAdmissionCoordinator {
    repository: ModelTurnAdmissionRepository,
    pending: Arc<Mutex<BTreeMap<String, ModelTurnLeaseIdentity>>>,
}

impl ModelTurnAdmissionCoordinator {
    #[must_use]
    pub fn new(repository: ModelTurnAdmissionRepository) -> Self {
        Self {
            repository,
            pending: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    /// Prepare an attempt; callers may launch provider I/O only from `Permit`.
    pub async fn prepare(
        &self,
        plan: &ProviderAttemptPlanV1,
        request: ModelTurnAdmissionRequest,
    ) -> djinn_db::Result<ModelTurnPreparation> {
        let Some(pool) = self
            .repository
            .resolve_pool(
                &request.credential_id,
                &plan.scope.provider_id,
                &plan.scope.model_id,
            )
            .await?
        else {
            return Ok(ModelTurnPreparation::Rejected(
                ModelTurnAdmissionRejection::PoolUnavailable,
            ));
        };
        match pool.phase {
            ModelTurnAdmissionPhase::Shadow => {
                // This durable, fingerprint-only record completes before permit hand-off.
                self.repository
                    .record_decision(ModelTurnDecisionRecordInput {
                        pool_id: pool.id,
                        request_fingerprint: request_fingerprint(&request.request_id),
                        generation: request.generation,
                        decision: ModelTurnDecisionKind::ShadowPermit,
                        diagnostic: None,
                    })
                    .await?;
                Ok(ModelTurnPreparation::Permit(ModelTurnSendPermit {
                    lease: None,
                }))
            }
            ModelTurnAdmissionPhase::Off => Ok(ModelTurnPreparation::Rejected(
                ModelTurnAdmissionRejection::Off,
            )),
            ModelTurnAdmissionPhase::Draining => {
                Ok(ModelTurnPreparation::Wait(ModelTurnAdmissionWait::Draining))
            }
            ModelTurnAdmissionPhase::Enforce => match self
                .repository
                .acquire_turn(ModelTurnAcquireInput {
                    pool_id: pool.id,
                    request_id: request.request_id.clone(),
                    owner_pod_uid: request.owner_pod_uid,
                    generation: request.generation,
                    debits: plan.debits.clone(),
                })
                .await?
            {
                ModelTurnAcquireOutcome::Wait(wait) => Ok(ModelTurnPreparation::Wait(wait)),
                ModelTurnAcquireOutcome::Rejected(rejection) => {
                    Ok(ModelTurnPreparation::Rejected(rejection))
                }
                ModelTurnAcquireOutcome::Admitted { lease, .. } => {
                    let identity = lease.identity;
                    // Insert before awaiting the fence. Dropping this future now cannot
                    // lose the only durable identity needed to refund an unsent lease.
                    self.pending
                        .lock()
                        .await
                        .insert(request.request_id, identity.clone());
                    let outcome = self.repository.mark_dispatching(&identity).await?;
                    match outcome {
                        ModelTurnLeaseMutationOutcome::Applied
                        | ModelTurnLeaseMutationOutcome::Idempotent => {
                            self.pending.lock().await.remove(&identity.request_id);
                            Ok(ModelTurnPreparation::Permit(ModelTurnSendPermit {
                                lease: Some(identity),
                            }))
                        }
                        ModelTurnLeaseMutationOutcome::Fenced => {
                            Ok(ModelTurnPreparation::DispatchFenced { identity, outcome })
                        }
                    }
                }
            },
        }
    }

    /// Cancellation owner for the acquisition-to-fence window. Repeating it is
    /// harmless; Phase A's terminal row makes the durable operation idempotent.
    pub async fn cancel_pending(
        &self,
        request_id: &str,
    ) -> djinn_db::Result<Option<ModelTurnLeaseMutationOutcome>> {
        let identity = self.pending.lock().await.remove(request_id);
        match identity {
            Some(identity) => self
                .repository
                .reconcile_lease(ModelTurnLeaseReconciliationInput {
                    identity,
                    outcome: ModelTurnLeaseTerminalOutcome::Cancelled,
                    authoritative_usage: None,
                    detail: None,
                })
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    pub async fn mark_active(
        &self,
        identity: &ModelTurnLeaseIdentity,
    ) -> djinn_db::Result<ModelTurnLeaseMutationOutcome> {
        self.repository.mark_active(identity).await
    }

    /// Sole coordinator reconciliation path. Missing authoritative usage is
    /// intentionally passed through as `None`, which quarantines possibly-sent spend.
    pub async fn reconcile(
        &self,
        identity: ModelTurnLeaseIdentity,
        outcome: &ProviderOutcomeV1,
    ) -> djinn_db::Result<ModelTurnLeaseMutationOutcome> {
        self.pending.lock().await.remove(&identity.request_id);
        let terminal = match outcome.terminal {
            ProviderAttemptTerminalV1::Completed => ModelTurnLeaseTerminalOutcome::Completed,
            ProviderAttemptTerminalV1::Aborted => ModelTurnLeaseTerminalOutcome::Cancelled,
            ProviderAttemptTerminalV1::Failed(_) => ModelTurnLeaseTerminalOutcome::Failed,
        };
        self.repository
            .reconcile_lease(ModelTurnLeaseReconciliationInput {
                identity,
                outcome: terminal,
                authoritative_usage: outcome.authoritative_usage.clone(),
                detail: None,
            })
            .await
    }
}

fn request_fingerprint(request_id: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(request_id.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_db::{Database, ModelTurnBucketDebit, ModelTurnBucketKind};
    use djinn_provider::{
        ProviderAbortCapabilityV1, ProviderAdmissionPolicyV1, ProviderAttemptAbortHandleV1,
        ProviderAttemptCapabilitiesV1, ProviderAttemptRouteCoverageV1, ProviderAttemptScopeV1,
        ProviderCredentialRecordScopeV1, ProviderHiddenRetryCapabilityV1,
        ProviderOutputReservationSourceV1,
    };

    async fn seed(db: &Database, phase: &str, capability: &str, available: i64) -> i64 {
        db.ensure_initialized().await.expect("initialize");
        sqlx::query("INSERT INTO credentials (id, provider_id, key_name, encrypted_value) VALUES ('credential-slot', 'provider', 'key-slot', decode('00', 'hex'))").execute(db.pool()).await.expect("credential");
        let pool = sqlx::query_scalar("INSERT INTO model_turn_pools (credential_id, provider_id, model_id, phase, capability_state, learned_concurrency) VALUES ('credential-slot', 'provider', 'model', $1, $2, 1) RETURNING id").bind(phase).bind(capability).fetch_one(db.pool()).await.expect("pool");
        sqlx::query("INSERT INTO model_turn_bucket_bindings (pool_id, bucket_kind, capacity_units, available_units) VALUES ($1, 'request', 2, $2)").bind(pool).bind(available).execute(db.pool()).await.expect("bucket");
        pool
    }
    fn plan() -> ProviderAttemptPlanV1 {
        ProviderAttemptPlanV1 {
            scope: ProviderAttemptScopeV1 {
                credential: ProviderCredentialRecordScopeV1::from_credential_record_id(
                    "credential-slot",
                ),
                provider_id: "provider".into(),
                model_id: "model".into(),
            },
            coverage: ProviderAttemptRouteCoverageV1::Covered {
                capabilities: ProviderAttemptCapabilitiesV1 {
                    hidden_retries: ProviderHiddenRetryCapabilityV1::Disabled,
                    abort: ProviderAbortCapabilityV1::Supported,
                },
                supported_bucket_bindings: vec![ModelTurnBucketKind::Request],
                policy: ProviderAdmissionPolicyV1::Proactive,
            },
            debits: vec![ModelTurnBucketDebit {
                bucket_kind: ModelTurnBucketKind::Request,
                units: 1,
            }],
            output_reservation_source: ProviderOutputReservationSourceV1::ExplicitLimit,
            abort: ProviderAttemptAbortHandleV1::new(),
        }
    }
    fn request(id: &str) -> ModelTurnAdmissionRequest {
        ModelTurnAdmissionRequest {
            credential_id: "credential-slot".into(),
            request_id: id.into(),
            owner_pod_uid: Some("pod".into()),
            generation: 1,
        }
    }

    #[tokio::test]
    async fn shadow_decision_is_redacted_bounded_and_written_before_permit() {
        let db = Database::ephemeral().await.expect("db");
        let pool = seed(&db, "shadow", "supported", 2).await;
        let coordinator =
            ModelTurnAdmissionCoordinator::new(ModelTurnAdmissionRepository::new(db.clone()));
        assert!(matches!(
            coordinator
                .prepare(&plan(), request("raw-secret-request"))
                .await
                .expect("prepare"),
            ModelTurnPreparation::Permit(ModelTurnSendPermit { lease: None })
        ));
        let row: (String, Option<String>) = sqlx::query_as(
            "SELECT request_fingerprint, diagnostic FROM model_turn_decisions WHERE pool_id = $1",
        )
        .bind(pool)
        .fetch_one(db.pool())
        .await
        .expect("record");
        assert_eq!(row.0, request_fingerprint("raw-secret-request"));
        assert_eq!(row.0.len(), 71);
        assert_eq!(row.1, None);
    }

    #[tokio::test]
    async fn typed_denial_has_no_partial_mutation_and_permit_is_fenced() {
        let db = Database::ephemeral().await.expect("db");
        let pool = seed(&db, "enforce", "degraded", 2).await;
        let coordinator =
            ModelTurnAdmissionCoordinator::new(ModelTurnAdmissionRepository::new(db.clone()));
        assert!(matches!(
            coordinator
                .prepare(&plan(), request("denied"))
                .await
                .expect("prepare"),
            ModelTurnPreparation::Rejected(
                ModelTurnAdmissionRejection::UnsupportedCapability { .. }
            )
        ));
        let before: (i64, i64) = sqlx::query_as("SELECT p.in_flight, b.available_units FROM model_turn_pools p JOIN model_turn_bucket_bindings b ON b.pool_id = p.id WHERE p.id = $1").bind(pool).fetch_one(db.pool()).await.expect("accounting");
        assert_eq!(before, (0, 2));
        sqlx::query("UPDATE model_turn_pools SET capability_state = 'supported' WHERE id = $1")
            .bind(pool)
            .execute(db.pool())
            .await
            .expect("support");
        assert!(matches!(
            coordinator
                .prepare(&plan(), request("fenced"))
                .await
                .expect("prepare"),
            ModelTurnPreparation::Permit(ModelTurnSendPermit { lease: Some(_) })
        ));
    }

    #[tokio::test]
    async fn pending_cancellation_refunds_unsent_and_sent_failure_quarantines_idempotently() {
        let db = Database::ephemeral().await.expect("db");
        let pool = seed(&db, "enforce", "supported", 2).await;
        let repository = ModelTurnAdmissionRepository::new(db.clone());
        let coordinator = ModelTurnAdmissionCoordinator::new(repository.clone());
        let acquired = repository
            .acquire_turn(ModelTurnAcquireInput {
                pool_id: pool,
                request_id: "cancelled".into(),
                owner_pod_uid: Some("pod".into()),
                generation: 1,
                debits: plan().debits,
            })
            .await
            .expect("acquire");
        let identity = match acquired {
            ModelTurnAcquireOutcome::Admitted { lease, .. } => lease.identity,
            _ => panic!("expected admission"),
        };
        coordinator
            .pending
            .lock()
            .await
            .insert("cancelled".into(), identity);
        assert_eq!(
            coordinator
                .cancel_pending("cancelled")
                .await
                .expect("cancel"),
            Some(ModelTurnLeaseMutationOutcome::Applied)
        );
        let refunded: (i64, i64, i64) = sqlx::query_as("SELECT p.in_flight, b.available_units, b.quarantined_units FROM model_turn_pools p JOIN model_turn_bucket_bindings b ON b.pool_id = p.id WHERE p.id = $1").bind(pool).fetch_one(db.pool()).await.expect("refund");
        assert_eq!(refunded, (0, 2, 0));
        let lease = match coordinator
            .prepare(&plan(), request("sent"))
            .await
            .expect("prepare")
        {
            ModelTurnPreparation::Permit(ModelTurnSendPermit {
                lease: Some(identity),
            }) => identity,
            _ => panic!("expected permit after fence"),
        };
        let outcome = ProviderOutcomeV1 {
            terminal: ProviderAttemptTerminalV1::Failed(
                djinn_provider::ProviderAttemptLossV1::Transport,
            ),
            authoritative_usage: None,
            observation: None,
            abort: djinn_provider::ProviderAttemptAbortResultV1::NotRequested,
            token_emission: Default::default(),
        };
        assert_eq!(
            coordinator
                .reconcile(lease.clone(), &outcome)
                .await
                .expect("terminal"),
            ModelTurnLeaseMutationOutcome::Applied
        );
        assert_eq!(
            coordinator
                .reconcile(lease, &outcome)
                .await
                .expect("replay"),
            ModelTurnLeaseMutationOutcome::Idempotent
        );
        let quarantined: (i64, i64) = sqlx::query_as("SELECT available_units, quarantined_units FROM model_turn_bucket_bindings WHERE pool_id = $1").bind(pool).fetch_one(db.pool()).await.expect("quarantine");
        assert_eq!(quarantined, (1, 1));
    }
}
