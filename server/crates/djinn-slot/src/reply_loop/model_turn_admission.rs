//! Slot-boundary admission. Provider I/O remains outside this coordinator.
//!
//! `prepare` is a send fence: it returns an enforced permit only after the
//! exact Phase A lease's dispatch transition has committed.

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use djinn_db::{
    ModelTurnAcquireInput, ModelTurnAcquireOutcome, ModelTurnAdmissionPhase,
    ModelTurnAdmissionRejection, ModelTurnAdmissionRepository, ModelTurnAdmissionWait,
    ModelTurnDecisionKind, ModelTurnDecisionRecordInput, ModelTurnLeaseIdentity,
    ModelTurnLeaseMutationOutcome, ModelTurnLeaseReconciliationInput,
    ModelTurnLeaseTerminalOutcome,
};
use djinn_provider::{ProviderAttemptPlanV1, ProviderAttemptTerminalV1, ProviderOutcomeV1};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, Notify};

#[cfg(test)]
use tokio::sync::Notify as TestNotify;

#[derive(Clone, Debug)]
pub struct ModelTurnAdmissionRequest {
    pub credential_id: String,
    pub request_id: String,
    pub owner_pod_uid: Option<String>,
    pub generation: i64,
}

/// An enforced permit owns its exact lease until the explicit provider-send hand-off.
pub struct ModelTurnSendPermit {
    pub lease: Option<ModelTurnLeaseIdentity>,
    ownership: Option<PreparationOwnership>,
}

impl std::fmt::Debug for ModelTurnSendPermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelTurnSendPermit")
            .field("lease", &self.lease)
            .finish_non_exhaustive()
    }
}

impl ModelTurnSendPermit {
    /// Commit the one-way hand-off before provider I/O can begin.
    pub async fn mark_active(&mut self) -> djinn_db::Result<ModelTurnLeaseMutationOutcome> {
        match &self.ownership {
            Some(ownership) => ownership.mark_active().await,
            None => Ok(ModelTurnLeaseMutationOutcome::Fenced),
        }
    }
}

#[derive(Debug)]
pub enum ModelTurnPreparation {
    Permit(ModelTurnSendPermit),
    Wait(ModelTurnAdmissionWait),
    Rejected(ModelTurnAdmissionRejection),
    DispatchFenced {
        identity: ModelTurnLeaseIdentity,
        outcome: ModelTurnLeaseMutationOutcome,
    },
}

/// The local guard survives every post-acquisition await and owns cancellation.
struct PreparationOwnership {
    repository: ModelTurnAdmissionRepository,
    state: Arc<Mutex<Option<ModelTurnLeaseIdentity>>>,
    cleanups: Arc<CleanupTracker>,
    #[cfg(test)]
    post_active_hook: Option<Arc<ActiveHandoffHook>>,
}

impl PreparationOwnership {
    fn new(
        repository: ModelTurnAdmissionRepository,
        identity: ModelTurnLeaseIdentity,
        cleanups: Arc<CleanupTracker>,
        #[cfg(test)] post_active_hook: Option<Arc<ActiveHandoffHook>>,
    ) -> Self {
        Self {
            repository,
            state: Arc::new(Mutex::new(Some(identity))),
            cleanups,
            #[cfg(test)]
            post_active_hook,
        }
    }

    async fn mark_active(&self) -> djinn_db::Result<ModelTurnLeaseMutationOutcome> {
        let mut state = self.state.lock().await;
        let Some(identity) = state.as_ref() else {
            return Ok(ModelTurnLeaseMutationOutcome::Fenced);
        };
        let outcome = self.repository.mark_active(identity).await?;
        if matches!(
            outcome,
            ModelTurnLeaseMutationOutcome::Applied | ModelTurnLeaseMutationOutcome::Idempotent
        ) {
            #[cfg(test)]
            if let Some(hook) = &self.post_active_hook {
                // Active is the durable hand-off linearization point. This
                // pause lets tests cancel exactly after it commits.
                hook.reached.notify_one();
                hook.release.notified().await;
            }
            // Active means provider send ownership can begin: refund is prohibited.
            *state = None;
        }
        Ok(outcome)
    }
}

impl Drop for PreparationOwnership {
    fn drop(&mut self) {
        let repository = self.repository.clone();
        let state = self.state.clone();
        let cleanups = self.cleanups.clone();
        cleanups.in_flight.fetch_add(1, Ordering::AcqRel);
        tokio::spawn(async move {
            // Retain this exact identity until the repository accepts a
            // fenced/idempotent cancellation result. Do not take it out of
            // shared ownership before an async repository call: a transient
            // database failure must leave the identity retained for retry.
            loop {
                let identity = state.lock().await.clone();
                let Some(identity) = identity else {
                    break;
                };
                match repository.cancel_before_send(identity).await {
                    Ok(_) => {
                        *state.lock().await = None;
                        break;
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(10)).await,
                }
            }
            cleanups.in_flight.fetch_sub(1, Ordering::AcqRel);
            cleanups.drained.notify_waiters();
        });
    }
}

#[derive(Default)]
struct CleanupTracker {
    in_flight: AtomicUsize,
    drained: Notify,
}

#[derive(Clone)]
pub struct ModelTurnAdmissionCoordinator {
    repository: ModelTurnAdmissionRepository,
    cleanups: Arc<CleanupTracker>,
    #[cfg(test)]
    post_dispatching_hook: Option<Arc<PrepareCancellationHook>>,
    #[cfg(test)]
    post_active_hook: Option<Arc<ActiveHandoffHook>>,
}

#[cfg(test)]
struct PrepareCancellationHook {
    reached: TestNotify,
    release: TestNotify,
}

#[cfg(test)]
struct ActiveHandoffHook {
    reached: TestNotify,
    release: TestNotify,
}

impl ModelTurnAdmissionCoordinator {
    #[must_use]
    pub fn new(repository: ModelTurnAdmissionRepository) -> Self {
        Self {
            repository,
            cleanups: Arc::new(CleanupTracker::default()),
            #[cfg(test)]
            post_dispatching_hook: None,
            #[cfg(test)]
            post_active_hook: None,
        }
    }
    #[cfg(test)]
    fn with_prepare_cancellation_hook(
        repository: ModelTurnAdmissionRepository,
        hook: Arc<PrepareCancellationHook>,
    ) -> Self {
        Self {
            repository,
            cleanups: Arc::new(CleanupTracker::default()),
            post_dispatching_hook: Some(hook),
            post_active_hook: None,
        }
    }
    #[cfg(test)]
    fn with_active_handoff_hook(
        repository: ModelTurnAdmissionRepository,
        hook: Arc<ActiveHandoffHook>,
    ) -> Self {
        Self {
            repository,
            cleanups: Arc::new(CleanupTracker::default()),
            post_dispatching_hook: None,
            post_active_hook: Some(hook),
        }
    }
    /// Join cancellation cleanup in deterministic tests.
    pub async fn wait_for_cleanup(&self) {
        loop {
            // Register before reading the count so a final notify cannot be
            // missed between the observation and the await.
            let notified = self.cleanups.drained.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.cleanups.in_flight.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
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
                    ownership: None,
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
                    // Guard exists before the dispatch fence await, so dropping this future retains exact identity.
                    let ownership = PreparationOwnership::new(
                        self.repository.clone(),
                        identity.clone(),
                        self.cleanups.clone(),
                        #[cfg(test)]
                        self.post_active_hook.clone(),
                    );
                    let outcome = self.repository.mark_dispatching(&identity).await?;
                    #[cfg(test)]
                    if let Some(hook) = &self.post_dispatching_hook {
                        hook.reached.notify_one();
                        hook.release.notified().await;
                    }
                    match outcome {
                        ModelTurnLeaseMutationOutcome::Applied
                        | ModelTurnLeaseMutationOutcome::Idempotent => {
                            Ok(ModelTurnPreparation::Permit(ModelTurnSendPermit {
                                lease: Some(identity),
                                ownership: Some(ownership),
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
    pub async fn reconcile(
        &self,
        identity: ModelTurnLeaseIdentity,
        outcome: &ProviderOutcomeV1,
    ) -> djinn_db::Result<ModelTurnLeaseMutationOutcome> {
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
            ModelTurnPreparation::Permit(ModelTurnSendPermit { lease: None, .. })
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
        // Raw UUIDs, credentials, and oversized values cannot be supplied: the
        // field accepts only ModelTurnDecisionDiagnostic.
        assert_eq!(
            djinn_db::ModelTurnDecisionDiagnostic::PoolUnavailable.code(),
            "pool_unavailable"
        );
        let count: i64 =
            sqlx::query_scalar("SELECT count(*) FROM model_turn_decisions WHERE pool_id = $1")
                .bind(pool)
                .fetch_one(db.pool())
                .await
                .expect("decision count");
        assert_eq!(count, 1, "unsafe diagnostics are never persisted");
    }

    fn repository(db: &Database) -> ModelTurnAdmissionRepository {
        ModelTurnAdmissionRepository::new(db.clone())
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
        let permit = match coordinator
            .prepare(&plan(), request("fenced"))
            .await
            .expect("prepare")
        {
            ModelTurnPreparation::Permit(permit) => permit,
            other => panic!("permit must follow a dispatch fence, got {other:?}"),
        };
        let lease = permit.lease.clone().expect("enforced lease");
        let lifecycle: String =
            sqlx::query_scalar("SELECT lifecycle FROM model_turn_leases WHERE lease_id = $1::uuid")
                .bind(&lease.lease_id)
                .fetch_one(db.pool())
                .await
                .expect("fenced lease");
        assert_eq!(lifecycle, "dispatching", "permit follows durable fence");
        drop(permit);
        coordinator.wait_for_cleanup().await;
    }

    #[tokio::test]
    async fn cancelling_actual_prepare_after_dispatch_fence_refunds_unsent() {
        let db = Database::ephemeral().await.expect("db");
        let pool = seed(&db, "enforce", "supported", 2).await;
        let hook = Arc::new(PrepareCancellationHook {
            reached: Notify::new(),
            release: Notify::new(),
        });
        let coordinator = Arc::new(
            ModelTurnAdmissionCoordinator::with_prepare_cancellation_hook(
                repository(&db),
                hook.clone(),
            ),
        );
        let reached = hook.reached.notified();
        let task = tokio::spawn({
            let coordinator = coordinator.clone();
            async move { coordinator.prepare(&plan(), request("cancelled")).await }
        });
        reached.await;
        let lifecycle: String = sqlx::query_scalar(
            "SELECT lifecycle FROM model_turn_leases WHERE request_id = 'cancelled'",
        )
        .fetch_one(db.pool())
        .await
        .expect("fence committed before cancellation");
        assert_eq!(lifecycle, "dispatching");
        task.abort();
        let _ = task.await;
        coordinator.wait_for_cleanup().await;
        let refunded: (i64, i64, i64) = sqlx::query_as("SELECT p.in_flight, b.available_units, b.quarantined_units FROM model_turn_pools p JOIN model_turn_bucket_bindings b ON b.pool_id = p.id WHERE p.id = $1").bind(pool).fetch_one(db.pool()).await.expect("refund");
        assert_eq!(refunded, (0, 2, 0));
    }

    #[tokio::test]
    async fn active_sent_failure_quarantines_idempotently() {
        let db = Database::ephemeral().await.expect("db");
        let pool = seed(&db, "enforce", "supported", 2).await;
        let coordinator = ModelTurnAdmissionCoordinator::new(repository(&db));
        let mut permit = match coordinator
            .prepare(&plan(), request("sent"))
            .await
            .expect("prepare")
        {
            ModelTurnPreparation::Permit(permit) => permit,
            _ => panic!("expected permit after fence"),
        };
        let lease = permit.lease.clone().expect("enforced lease");
        assert_eq!(
            permit.mark_active().await.expect("active"),
            ModelTurnLeaseMutationOutcome::Applied
        );
        // A drop racing the completed hand-off sees cleared ownership and cannot
        // refund the now possibly-sent active lease.
        drop(permit);
        coordinator.wait_for_cleanup().await;
        let lifecycle: String =
            sqlx::query_scalar("SELECT lifecycle FROM model_turn_leases WHERE lease_id = $1::uuid")
                .bind(&lease.lease_id)
                .fetch_one(db.pool())
                .await
                .expect("active lease retained");
        assert_eq!(lifecycle, "active");
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

    #[tokio::test]
    async fn cancelled_handoff_racing_terminal_never_refunds_active_lease() {
        let db = Database::ephemeral().await.expect("db");
        let pool = seed(&db, "enforce", "supported", 2).await;
        let hook = Arc::new(ActiveHandoffHook {
            reached: Notify::new(),
            release: Notify::new(),
        });
        let coordinator = Arc::new(ModelTurnAdmissionCoordinator::with_active_handoff_hook(
            repository(&db),
            hook.clone(),
        ));
        let permit = match coordinator
            .prepare(&plan(), request("handoff-race"))
            .await
            .expect("prepare")
        {
            ModelTurnPreparation::Permit(permit) => permit,
            _ => panic!("expected permit after fence"),
        };
        let lease = permit.lease.clone().expect("enforced lease");

        // The hook is after the durable active transition and before local
        // ownership is cleared. Aborting here forces permit drop/cancellation
        // to race an independently-started terminal reconciliation.
        let reached = hook.reached.notified();
        let handoff = tokio::spawn(async move {
            let mut permit = permit;
            permit.mark_active().await
        });
        reached.await;
        let terminal = tokio::spawn({
            let coordinator = coordinator.clone();
            let lease = lease.clone();
            async move {
                let outcome = ProviderOutcomeV1 {
                    terminal: ProviderAttemptTerminalV1::Failed(
                        djinn_provider::ProviderAttemptLossV1::Transport,
                    ),
                    authoritative_usage: None,
                    observation: None,
                    abort: djinn_provider::ProviderAttemptAbortResultV1::NotRequested,
                    token_emission: Default::default(),
                };
                coordinator.reconcile(lease, &outcome).await
            }
        });
        handoff.abort();
        assert!(
            handoff
                .await
                .expect_err("handoff must be cancelled")
                .is_cancelled()
        );
        coordinator.wait_for_cleanup().await;
        let terminal_outcome = terminal.await.expect("terminal task").expect("terminal");
        assert!(matches!(
            terminal_outcome,
            ModelTurnLeaseMutationOutcome::Applied
                | ModelTurnLeaseMutationOutcome::Idempotent
                | ModelTurnLeaseMutationOutcome::Fenced
        ));

        let accounting: (i64, i64, i64) = sqlx::query_as("SELECT p.in_flight, b.available_units, b.quarantined_units FROM model_turn_pools p JOIN model_turn_bucket_bindings b ON b.pool_id = p.id WHERE p.id = $1")
            .bind(pool)
            .fetch_one(db.pool())
            .await
            .expect("active attempt is never refunded");
        assert_eq!(accounting, (0, 1, 1));
        let terminal: (String, String) = sqlx::query_as("SELECT outcome, accounting_state FROM model_turn_lease_terminals WHERE lease_id = $1::uuid AND generation = $2 AND request_id = $3")
            .bind(&lease.lease_id)
            .bind(lease.generation)
            .bind(&lease.request_id)
            .fetch_one(db.pool())
            .await
            .expect("terminal uses the exact lease identity");
        assert!(matches!(terminal.0.as_str(), "failed" | "cancelled"));
        assert_eq!(terminal.1, "quarantined");
    }
}
