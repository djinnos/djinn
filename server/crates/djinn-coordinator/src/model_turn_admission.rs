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
use djinn_k8s::{WorkloadObjectKind, WorkloadRecord};
use djinn_provider::{ProviderAttemptPlanV1, ProviderOutcomeV1};
use djinn_slot::model_turn_capability::{
    ModelTurnCapabilityCoverageV2, ModelTurnCapabilityReportV2,
};
use sha2::{Digest, Sha256};

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
/// denominator. Covered reports become positive capability heartbeats; every
/// joined report is retained as bounded heartbeat-stage evidence, with
/// `Missing` representing an explicitly uncovered route.
///
/// This retains negative capability evidence without treating an uncovered
/// report as an indistinguishable positive heartbeat. The repository verifies
/// pool identity and route labels again.
pub async fn persist_joined_capability_reports_v1(
    repository: &ModelTurnAdmissionRepository,
    projection: &ExpectedAttemptPathProjectionV1,
) -> djinn_db::Result<()> {
    for joined in &projection.joined_reports {
        let outcome = match joined.coverage {
            ModelTurnCapabilityCoverageV2::Covered => {
                repository
                    .record_capability_heartbeat(ModelTurnCapabilityHeartbeatInput {
                        pool_id: joined.expected_path.pool_id,
                        slot_pod_uid: joined.expected_path.slot_pod_uid.clone(),
                        deployment_revision: joined.expected_path.deployment_revision.clone(),
                        provider_id: joined.expected_path.provider.clone(),
                        model_id: joined.expected_path.model_scope.clone(),
                    })
                    .await?;
                ModelTurnPhaseCEvidenceOutcome::Recorded
            }
            ModelTurnCapabilityCoverageV2::Uncovered => ModelTurnPhaseCEvidenceOutcome::Missing,
        };
        persist_expected_path_evidence_v1(
            repository,
            &joined.expected_path,
            capability_report_fingerprint(&joined.expected_path),
            ModelTurnPhaseCEvidenceStage::Heartbeat,
            outcome,
        )
        .await?;
    }
    Ok(())
}

fn capability_report_fingerprint(expected_path: &ExpectedAttemptPathV1) -> String {
    let mut digest = Sha256::new();
    for value in [
        &expected_path.slot_pod_uid,
        &expected_path.deployment_revision,
        &expected_path.provider,
        &expected_path.model_scope,
    ] {
        digest.update(value.as_bytes());
        digest.update([0]);
    }
    format!("sha256:{:x}", digest.finalize())
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
    // A B2 report has no credential identity, so a single exact four-field
    // report corroborates every already-resolved credential-qualified route
    // sharing that tuple. Never select an arbitrary first pool here.
    let joined_reports = reports
        .iter()
        .flat_map(|report| {
            expected_paths
                .iter()
                .filter(move |path| {
                    path.slot_pod_uid == report.slot_pod_uid
                        && path.deployment_revision == report.deployment_revision
                        && path.provider == report.provider
                        && path.model_scope == report.model_scope
                })
                .cloned()
                .map(move |expected_path| JoinedCapabilityReportV1 {
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
    use djinn_db::{Database, ModelTurnBucketDebit, ModelTurnBucketKind};
    use djinn_provider::{
        ProviderAttemptAbortHandleV1, ProviderAttemptAbortResultV1, ProviderAttemptLossV1,
        ProviderAttemptRouteCoverageV1, ProviderAttemptScopeV1, ProviderAttemptTerminalV1,
        ProviderCredentialRecordScopeV1, ProviderOutputReservationSourceV1,
    };
    use std::collections::BTreeMap;

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
        sqlx::query_scalar::<_, i64>("INSERT INTO model_turn_pools (credential_id, provider_id, model_id, phase, identity_state, capability_state, learned_concurrency, in_flight) VALUES ($1, $2, $3, 'shadow', 'eligible', 'supported', 1, 0) RETURNING id")
            .bind(credential)
            .bind(provider)
            .bind(model)
            .fetch_one(db.pool())
            .await
            .expect("seed")
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
    async fn reports_preserve_coverage_and_persist_complete_evidence_surface() {
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
        let mut reports = vec![
            report.clone(),
            ModelTurnCapabilityReportV2 {
                coverage: ModelTurnCapabilityCoverageV2::Uncovered,
                ..report.clone()
            },
        ];
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
        assert_eq!(projection.joined_reports.len(), 2);
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
            1,
            "only covered reports are positive capability heartbeats"
        );
        let path = &projection.expected_paths[0];
        let fingerprint = format!("sha256:{}", "a".repeat(64));
        for stage in [
            ModelTurnPhaseCEvidenceStage::Decision,
            ModelTurnPhaseCEvidenceStage::Dispatch,
            ModelTurnPhaseCEvidenceStage::Reconcile,
        ] {
            persist_expected_path_evidence_v1(
                &repository,
                path,
                fingerprint.clone(),
                stage,
                ModelTurnPhaseCEvidenceOutcome::Recorded,
            )
            .await
            .expect("stage persists");
        }
        persist_provider_outcome_v1(
            &repository,
            path,
            fingerprint,
            &ProviderOutcomeV1 {
                terminal: ProviderAttemptTerminalV1::Failed(ProviderAttemptLossV1::Transport),
                authoritative_usage: None,
                observation: None,
                abort: ProviderAttemptAbortResultV1::NotRequested,
                token_emission: Default::default(),
            },
        )
        .await
        .expect("provider outcome persists");
        let evidence = repository
            .recent_phase_c_evidence(pool_id, 8)
            .await
            .expect("read evidence");
        assert_eq!(evidence.len(), 6);
        assert!(evidence.iter().any(|row| {
            row.stage == "heartbeat" && row.outcome == "recorded" && row.provider_id == "provider"
        }));
        assert!(evidence.iter().any(|row| {
            row.stage == "heartbeat" && row.outcome == "missing" && row.model_id == "model"
        }));
        assert!(
            evidence
                .iter()
                .any(|row| { row.stage == "provider_outcome" && row.outcome == "failed" })
        );
        for stage in [
            "decision",
            "dispatch",
            "heartbeat",
            "provider_outcome",
            "reconcile",
        ] {
            assert!(
                evidence.iter().any(|row| row.stage == stage),
                "{stage} is consumable by the next Phase-C window"
            );
        }
    }

    #[tokio::test]
    async fn one_exact_report_persists_evidence_for_every_resolved_credential_route() {
        let db = Database::ephemeral().await.expect("db");
        let first_pool_id = seed(&db, "credential-a", "provider", "model").await;
        let second_pool_id = seed(&db, "credential-b", "provider", "model").await;
        let repository = ModelTurnAdmissionRepository::new(db);
        let projection = project_from_inventory(
            &repository,
            vec![record(Some("slot-a"), Some("rev-1"), true, false)],
            &[
                plan("credential-a", "provider", "model"),
                plan("credential-b", "provider", "model"),
            ],
            &[ModelTurnCapabilityReportV2 {
                slot_pod_uid: "slot-a".into(),
                deployment_revision: "rev-1".into(),
                provider: "provider".into(),
                model_scope: "model".into(),
                coverage: ModelTurnCapabilityCoverageV2::Covered,
            }],
        )
        .await
        .expect("projection");

        assert_eq!(projection.expected_paths.len(), 2);
        assert_eq!(projection.joined_reports.len(), 2);
        assert_eq!(
            projection
                .joined_reports
                .iter()
                .map(|joined| joined.expected_path.pool_id)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([first_pool_id, second_pool_id]),
            "the exact report joins every credential-qualified pool route"
        );

        persist_joined_capability_reports_v1(&repository, &projection)
            .await
            .expect("persist");
        for pool_id in [first_pool_id, second_pool_id] {
            assert_eq!(
                repository
                    .recent_capability_heartbeats(pool_id, 8)
                    .await
                    .expect("heartbeat read")
                    .len(),
                1,
                "covered report is retained under its exact resolved pool"
            );
            let evidence = repository
                .recent_phase_c_evidence(pool_id, 8)
                .await
                .expect("evidence read");
            assert_eq!(evidence.len(), 1);
            assert_eq!(evidence[0].stage, "heartbeat");
            assert_eq!(evidence[0].outcome, "recorded");
        }
    }
}
