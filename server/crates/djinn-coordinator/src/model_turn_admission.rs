//! Coordinator-owned Phase-C expected-path and evidence projection.
//!
//! The denominator is assembled from the actor's live workload inventory and
//! B1 routes which resolve to an existing durable pool. B2 reports are joined
//! after that construction and therefore cannot create expected members.

use std::collections::BTreeSet;

use djinn_db::{
    ModelTurnAdmissionRepository, ModelTurnCapabilityHeartbeatInput, ModelTurnPhaseCEvidenceInput,
    ModelTurnPhaseCEvidenceOutcome, ModelTurnPhaseCEvidenceStage, ModelTurnPool,
};
use djinn_k8s::{WorkloadInventory, WorkloadObjectKind, WorkloadRecord};
use djinn_provider::{ProviderAttemptPlanV1, ProviderOutcomeV1};
use djinn_slot::model_turn_capability::{
    ModelTurnCapabilityCoverageV2, ModelTurnCapabilityReportV2,
};

use crate::CoordinatorActor;

/// One trusted expected Phase-C route at one Ready live slot.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExpectedAttemptPathV1 {
    pub slot_pod_uid: String,
    pub deployment_revision: String,
    pub provider: String,
    pub model_scope: String,
    /// The exact existing admitted route selected through the B1 credential
    /// record scope. It is retained for every bounded persistence operation.
    pub pool_id: i64,
}

/// A capability report that exact-joined an already expected route.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JoinedCapabilityReportV1 {
    pub expected_path: ExpectedAttemptPathV1,
    pub coverage: ModelTurnCapabilityCoverageV2,
}

/// The denominator and corroborating B2 evidence for a Phase-C window.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExpectedAttemptPathProjectionV1 {
    pub expected_paths: Vec<ExpectedAttemptPathV1>,
    pub joined_reports: Vec<JoinedCapabilityReportV1>,
}

impl CoordinatorActor {
    /// Build the Phase-C projection from this actor's existing workload
    /// inventory. Missing inventory fails closed to an empty denominator.
    pub async fn project_expected_attempt_paths_v1(
        &self,
        plans: &[ProviderAttemptPlanV1],
        reports: &[ModelTurnCapabilityReportV2],
    ) -> djinn_db::Result<ExpectedAttemptPathProjectionV1> {
        let Some(inventory) = self.workload_inventory.as_ref() else {
            return Ok(ExpectedAttemptPathProjectionV1::default());
        };
        let records = inventory
            .list()
            .await
            .map_err(djinn_db::Error::InvalidData)?;
        let repository = ModelTurnAdmissionRepository::new(self.db.clone());
        project_from_inventory(&repository, records, plans, reports).await
    }
}

/// Persist B2 evidence only if it exact-joined the coordinator-owned expected
/// denominator. The repository verifies pool identity and route labels again.
pub async fn persist_joined_capability_reports_v1(
    repository: &ModelTurnAdmissionRepository,
    projection: &ExpectedAttemptPathProjectionV1,
) -> djinn_db::Result<()> {
    for joined in &projection.joined_reports {
        repository
            .record_capability_heartbeat(ModelTurnCapabilityHeartbeatInput {
                pool_id: joined.expected_path.pool_id,
                slot_pod_uid: joined.expected_path.slot_pod_uid.clone(),
                deployment_revision: joined.expected_path.deployment_revision.clone(),
                provider_id: joined.expected_path.provider.clone(),
                model_id: joined.expected_path.model_scope.clone(),
            })
            .await?;
    }
    Ok(())
}

/// Persist any bounded Phase-C decision, dispatch, heartbeat, provider-outcome,
/// or reconciliation stage at the exact resolved expected route.
pub async fn persist_expected_path_evidence_v1(
    repository: &ModelTurnAdmissionRepository,
    expected_path: &ExpectedAttemptPathV1,
    attempt_fingerprint: String,
    stage: ModelTurnPhaseCEvidenceStage,
    outcome: ModelTurnPhaseCEvidenceOutcome,
) -> djinn_db::Result<()> {
    repository
        .record_phase_c_evidence(ModelTurnPhaseCEvidenceInput {
            pool_id: expected_path.pool_id,
            slot_pod_uid: expected_path.slot_pod_uid.clone(),
            deployment_revision: expected_path.deployment_revision.clone(),
            provider_id: expected_path.provider.clone(),
            model_id: expected_path.model_scope.clone(),
            attempt_fingerprint,
            stage,
            outcome,
        })
        .await
}

/// Persist the provider outcome at the exact resolved expected route. The
/// caller supplies B1's vocabulary; this projection never reclassifies it.
pub async fn persist_provider_outcome_v1(
    repository: &ModelTurnAdmissionRepository,
    expected_path: &ExpectedAttemptPathV1,
    attempt_fingerprint: String,
    outcome: &ProviderOutcomeV1,
) -> djinn_db::Result<()> {
    let evidence_outcome = match outcome.terminal {
        djinn_provider::ProviderAttemptTerminalV1::Completed => {
            ModelTurnPhaseCEvidenceOutcome::Succeeded
        }
        djinn_provider::ProviderAttemptTerminalV1::Failed(_)
        | djinn_provider::ProviderAttemptTerminalV1::Aborted => {
            ModelTurnPhaseCEvidenceOutcome::Failed
        }
    };
    persist_expected_path_evidence_v1(
        repository,
        expected_path,
        attempt_fingerprint,
        ModelTurnPhaseCEvidenceStage::ProviderOutcome,
        evidence_outcome,
    )
    .await
}

async fn project_from_inventory(
    repository: &ModelTurnAdmissionRepository,
    records: Vec<WorkloadRecord>,
    plans: &[ProviderAttemptPlanV1],
    reports: &[ModelTurnCapabilityReportV2],
) -> djinn_db::Result<ExpectedAttemptPathProjectionV1> {
    let routes = resolve_planned_routes(repository, plans).await?;
    let mut expected_paths = BTreeSet::new();
    for record in records.iter().filter(eligible_live_slot) {
        let Some(uid) = record
            .uid
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            continue;
        };
        let Some(revision) = record
            .deployment_revision
            .as_deref()
            .filter(|value| !value.trim().is_empty())
        else {
            continue;
        };
        for pool in &routes {
            expected_paths.insert(ExpectedAttemptPathV1 {
                slot_pod_uid: uid.to_owned(),
                deployment_revision: revision.to_owned(),
                provider: pool.provider_id.clone(),
                model_scope: pool.model_id.clone(),
                pool_id: pool.id,
            });
        }
    }
    let expected_paths: Vec<_> = expected_paths.into_iter().collect();
    let joined_reports = reports
        .iter()
        .filter_map(|report| {
            expected_paths
                .iter()
                .find(|path| {
                    path.slot_pod_uid == report.slot_pod_uid
                        && path.deployment_revision == report.deployment_revision
                        && path.provider == report.provider
                        && path.model_scope == report.model_scope
                })
                .cloned()
                .map(|expected_path| JoinedCapabilityReportV1 {
                    expected_path,
                    coverage: report.coverage,
                })
        })
        .collect();
    Ok(ExpectedAttemptPathProjectionV1 {
        expected_paths,
        joined_reports,
    })
}

fn eligible_live_slot(record: &&WorkloadRecord) -> bool {
    record.kind == WorkloadObjectKind::Pod && record.ready && !record.terminal
}

async fn resolve_planned_routes(
    repository: &ModelTurnAdmissionRepository,
    plans: &[ProviderAttemptPlanV1],
) -> djinn_db::Result<Vec<ModelTurnPool>> {
    let mut routes = Vec::new();
    for plan in plans {
        // The B1 credential scope is deliberately opaque. Resolve only through
        // the existing admitted pool and require the returned route to agree.
        let fingerprint = plan.scope.credential.fingerprint();
        let Some(pool) = repository
            .resolve_pool_by_credential_fingerprint(
                fingerprint,
                &plan.scope.provider_id,
                &plan.scope.model_id,
            )
            .await?
        else {
            continue;
        };
        routes.push(pool);
    }
    routes.sort_by_key(|pool| pool.id);
    routes.dedup_by_key(|pool| pool.id);
    Ok(routes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use djinn_db::{Database, ModelTurnBucketDebit, ModelTurnBucketKind};
    use djinn_provider::{
        ProviderAttemptAbortHandleV1, ProviderAttemptRouteCoverageV1, ProviderAttemptScopeV1,
        ProviderCredentialRecordScopeV1, ProviderOutputReservationSourceV1,
    };
    use std::collections::BTreeMap;

    #[derive(Clone)]
    struct Inventory(Vec<WorkloadRecord>);
    #[async_trait]
    impl WorkloadInventory for Inventory {
        async fn list(&self) -> Result<Vec<WorkloadRecord>, String> {
            Ok(self.0.clone())
        }
        async fn get_uid(
            &self,
            _: WorkloadObjectKind,
            _: &str,
            _: &str,
        ) -> djinn_k8s::UidGetResult {
            djinn_k8s::UidGetResult::Present
        }
        async fn presence(&self, _: WorkloadObjectKind, _: &str) -> djinn_k8s::ObjectPresence {
            djinn_k8s::ObjectPresence::Absent
        }
    }
    fn record(
        uid: Option<&str>,
        revision: Option<&str>,
        ready: bool,
        terminal: bool,
    ) -> WorkloadRecord {
        WorkloadRecord {
            kind: WorkloadObjectKind::Pod,
            name: "slot".into(),
            uid: uid.map(str::to_owned),
            labels: BTreeMap::new(),
            terminal,
            ready,
            deployment_revision: revision.map(str::to_owned),
            images: vec![],
            commands: vec![],
        }
    }
    fn plan(credential: &str, provider: &str, model: &str) -> ProviderAttemptPlanV1 {
        ProviderAttemptPlanV1 {
            scope: ProviderAttemptScopeV1 {
                credential: ProviderCredentialRecordScopeV1::from_credential_record_id(credential),
                provider_id: provider.into(),
                model_id: model.into(),
            },
            coverage: ProviderAttemptRouteCoverageV1::Uncovered(
                djinn_provider::ProviderAttemptUncoveredReasonV1::SerializationUnavailable,
            ),
            debits: vec![ModelTurnBucketDebit {
                bucket_kind: ModelTurnBucketKind::Request,
                units: 1,
            }],
            output_reservation_source: ProviderOutputReservationSourceV1::ExplicitLimit,
            abort: ProviderAttemptAbortHandleV1::new(),
        }
    }
    async fn seed(db: &Database, credential: &str, provider: &str, model: &str) -> i64 {
        db.ensure_initialized().await.expect("initialize");
        sqlx::query("INSERT INTO model_turn_pools (credential_id, provider_id, model_id, phase, identity_state, capability_state, learned_concurrency, in_flight) VALUES ($1, $2, $3, 'shadow', 'eligible', 'supported', 1, 0) RETURNING id").bind(credential).bind(provider).bind(model).fetch_one(db.pool()).await.expect("seed")
    }
    #[tokio::test]
    async fn denominator_comes_only_from_live_slots_and_resolved_routes() {
        let db = Database::ephemeral().await.expect("db");
        let pool_id = seed(&db, "credential-a", "provider", "model").await;
        let projection = project_from_inventory(
            &ModelTurnAdmissionRepository::new(db),
            vec![
                record(Some("ready-old"), Some("rev-1"), true, false),
                record(Some("ready-new"), Some("rev-2"), true, false),
                record(Some("not-ready"), Some("rev-3"), false, false),
                record(Some("terminal"), Some("rev-4"), true, true),
                record(None, Some("rev-5"), true, false),
                record(Some("missing-revision"), None, true, false),
            ],
            &[
                plan("credential-a", "provider", "model"),
                plan("fabricated", "attacker", "model"),
            ],
            &[],
        )
        .await
        .expect("projection");
        assert_eq!(projection.expected_paths.len(), 2);
        assert!(
            projection
                .expected_paths
                .iter()
                .all(|path| path.pool_id == pool_id)
        );
        assert!(
            projection.joined_reports.is_empty(),
            "silent paths remain expected"
        );
    }
    #[tokio::test]
    async fn reports_are_exact_evidence_only_and_persist_resolved_pool() {
        let db = Database::ephemeral().await.expect("db");
        let pool_id = seed(&db, "credential-a", "provider", "model").await;
        let repository = ModelTurnAdmissionRepository::new(db);
        let report = ModelTurnCapabilityReportV2 {
            slot_pod_uid: "slot-a".into(),
            deployment_revision: "rev-1".into(),
            provider: "provider".into(),
            model_scope: "model".into(),
            coverage: ModelTurnCapabilityCoverageV2::Covered,
        };
        let mut reports = vec![report.clone()];
        for mismatch in [
            ModelTurnCapabilityReportV2 {
                slot_pod_uid: "other-slot".into(),
                ..report.clone()
            },
            ModelTurnCapabilityReportV2 {
                deployment_revision: "other-revision".into(),
                ..report.clone()
            },
            ModelTurnCapabilityReportV2 {
                provider: "attacker".into(),
                ..report.clone()
            },
            ModelTurnCapabilityReportV2 {
                model_scope: "attacker-model".into(),
                ..report.clone()
            },
        ] {
            reports.push(mismatch);
        }
        let projection = project_from_inventory(
            &repository,
            vec![record(Some("slot-a"), Some("rev-1"), true, false)],
            &[plan("credential-a", "provider", "model")],
            &reports,
        )
        .await
        .expect("projection");
        assert_eq!(projection.expected_paths.len(), 1);
        assert_eq!(projection.joined_reports.len(), 1);
        assert_eq!(projection.joined_reports[0].expected_path.pool_id, pool_id);
        persist_joined_capability_reports_v1(&repository, &projection)
            .await
            .expect("persist");
        assert_eq!(
            repository
                .recent_capability_heartbeats(pool_id, 2)
                .await
                .expect("read")
                .len(),
            1
        );
    }
}
