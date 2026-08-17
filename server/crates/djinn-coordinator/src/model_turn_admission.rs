//! Coordinator-owned Phase-C expected-path and evidence projection.
//!
//! The denominator is assembled from the actor's live workload inventory and
//! B1 routes which resolve to an existing durable pool. B2 reports are joined
//! after that construction and therefore cannot create expected members.

use std::collections::{BTreeMap, BTreeSet};

use djinn_db::{
    ModelTurnAdmissionRepository, ModelTurnCapabilityHeartbeatInput, ModelTurnPhaseCEvidenceInput,
    ModelTurnPhaseCEvidenceOutcome, ModelTurnPhaseCEvidenceStage, ModelTurnPool,
};
use djinn_k8s::{WorkloadObjectKind, WorkloadRecord};
use djinn_provider::{ProviderAttemptPlanV1, ProviderOutcomeV1, catalog::CatalogService};
use djinn_slot::model_turn_capability::{
    ModelTurnCapabilityCoverageV2, ModelTurnCapabilityReportV2,
};
use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::CoordinatorActor;

/// One trusted expected Phase-C route at one Ready live slot.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ExpectedAttemptPathV1 {
    slot_pod_uid: String,
    deployment_revision: String,
    provider: String,
    model_scope: String,
    /// The exact existing admitted route selected through the B1 credential
    /// record scope. It is retained for every bounded persistence operation.
    pool_id: i64,
}

impl ExpectedAttemptPathV1 {
    /// Build a path only from a coordinator-resolved pool route. Private route
    /// fields prevent reports and qualifier callers from inventing arbitrary
    /// provider/model labels in the authoritative denominator.
    fn from_resolved_route(
        slot_pod_uid: String,
        deployment_revision: String,
        pool: &ModelTurnPool,
    ) -> Self {
        Self {
            slot_pod_uid,
            deployment_revision,
            provider: pool.provider_id.clone(),
            model_scope: pool.model_id.clone(),
            pool_id: pool.id,
        }
    }

    #[cfg(test)]
    fn test_resolved_route(pool_id: i64) -> Self {
        Self {
            slot_pod_uid: "slot-catalog-bound".into(),
            deployment_revision: "revision-catalog-bound".into(),
            provider: "provider-catalog-bound".into(),
            model_scope: "model-catalog-bound".into(),
            pool_id,
        }
    }
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
        project_from_inventory(&repository, &self.catalog, records, plans, reports).await
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
    catalog: &CatalogService,
    records: Vec<WorkloadRecord>,
    plans: &[ProviderAttemptPlanV1],
    reports: &[ModelTurnCapabilityReportV2],
) -> djinn_db::Result<ExpectedAttemptPathProjectionV1> {
    let routes = resolve_planned_routes(repository, catalog, plans).await?;
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
            expected_paths.insert(ExpectedAttemptPathV1::from_resolved_route(
                uid.to_owned(),
                revision.to_owned(),
                pool,
            ));
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

/// A controller window is always one aligned, half-open minute. Private bounds
/// make malformed 61-second and unaligned windows unrepresentable to callers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AlignedPhaseCWindowV1 {
    start_second: i64,
    end_second: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AlignedPhaseCWindowErrorV1 {
    NegativeStart,
    UnalignedStart,
    EndOverflow,
}

impl AlignedPhaseCWindowV1 {
    pub const SECONDS: i64 = 60;
    pub fn new(start_second: i64) -> Result<Self, AlignedPhaseCWindowErrorV1> {
        if start_second < 0 {
            return Err(AlignedPhaseCWindowErrorV1::NegativeStart);
        }
        if start_second % Self::SECONDS != 0 {
            return Err(AlignedPhaseCWindowErrorV1::UnalignedStart);
        }
        let end_second = start_second
            .checked_add(Self::SECONDS)
            .ok_or(AlignedPhaseCWindowErrorV1::EndOverflow)?;
        Ok(Self {
            start_second,
            end_second,
        })
    }
    #[must_use]
    pub fn start_second(self) -> i64 {
        self.start_second
    }
    #[must_use]
    pub fn end_second(self) -> i64 {
        self.end_second
    }
    fn contains(self, second: i64) -> bool {
        self.start_second <= second && second < self.end_second
    }
    fn fully_covers(self, start_second: i64, end_second: i64) -> bool {
        start_second <= self.start_second && self.end_second <= end_second
    }
}

/// Observation time is separate from asserted coverage, allowing fresh
/// evidence whose coverage interval crosses a window boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhaseCCapabilityEvidenceV1 {
    pub path: ExpectedAttemptPathV1,
    pub coverage_start_second: i64,
    pub coverage_end_second: i64,
    pub observed_at_second: i64,
    pub covered: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum PhaseCAttemptStageV1 {
    Decision,
    Dispatch,
    Heartbeat,
    ProviderOutcome,
    Reconcile,
}

/// `Missing` is retained but never constitutes a complete stage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PhaseCAttemptEvidenceOutcomeV1 {
    Recorded,
    Provider(Box<ProviderOutcomeV1>),
    Missing,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhaseCAttemptStageEvidenceV1 {
    pub stage: PhaseCAttemptStageV1,
    pub timestamp_second: i64,
    pub outcome: PhaseCAttemptEvidenceOutcomeV1,
}

/// The fingerprint join key is deliberately absent from this in-memory input:
/// stages are already grouped under exactly one admitted attempt by the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhaseCAdmittedAttemptV1 {
    pub path: ExpectedAttemptPathV1,
    pub admitted_at_second: i64,
    pub has_authoritative_usage: bool,
    pub lease_expired: bool,
    pub breaker_open: bool,
    pub stages: Vec<PhaseCAttemptStageEvidenceV1>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseCWindowDiagnosticCodeV1 {
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

/// Bounded redaction-safe diagnostic. Pool 0 is the fixed sentinel for evidence
/// that cannot join an authoritative pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct PhaseCWindowDiagnosticV1 {
    pub pool_id: i64,
    pub code: PhaseCWindowDiagnosticCodeV1,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PhaseCWindowQualificationV1 {
    pub admitted: bool,
    pub diagnostics: Vec<PhaseCWindowDiagnosticV1>,
}

/// Pure fail-closed qualification over the coordinator-owned expected-path
/// denominator. Observational evidence may corroborate it, never extend it.
#[must_use]
pub fn qualify_aligned_phase_c_window_v1(
    window: AlignedPhaseCWindowV1,
    expected_paths: &[ExpectedAttemptPathV1],
    capability_evidence: &[PhaseCCapabilityEvidenceV1],
    admitted_attempts: &[PhaseCAdmittedAttemptV1],
) -> PhaseCWindowQualificationV1 {
    let expected: BTreeSet<_> = expected_paths.iter().cloned().collect();
    let mut diagnostics = BTreeSet::new();
    if expected.is_empty() {
        diagnostics.insert(PhaseCWindowDiagnosticV1 {
            pool_id: 0,
            code: PhaseCWindowDiagnosticCodeV1::EmptyExpectedDenominator,
        });
    }
    let mut reports: BTreeMap<_, Vec<&PhaseCCapabilityEvidenceV1>> = BTreeMap::new();
    for report in capability_evidence {
        if expected.contains(&report.path) {
            reports.entry(report.path.clone()).or_default().push(report);
        } else {
            diagnostics.insert(PhaseCWindowDiagnosticV1 {
                pool_id: 0,
                code: PhaseCWindowDiagnosticCodeV1::UnexpectedCapability,
            });
        }
    }
    for path in &expected {
        let matched = reports.get(path).map(Vec::as_slice).unwrap_or_default();
        let code = match matched {
            [] => Some(PhaseCWindowDiagnosticCodeV1::MissingCapability),
            [_first, _second, ..] => Some(PhaseCWindowDiagnosticCodeV1::DuplicateCapability),
            [report] if !report.covered => Some(PhaseCWindowDiagnosticCodeV1::UncoveredCapability),
            [report]
                if !window
                    .fully_covers(report.coverage_start_second, report.coverage_end_second) =>
            {
                Some(PhaseCWindowDiagnosticCodeV1::PartialCapabilityCoverage)
            }
            [report] if !window.contains(report.observed_at_second) => {
                Some(PhaseCWindowDiagnosticCodeV1::StaleHeartbeat)
            }
            [_] => None,
        };
        if let Some(code) = code {
            diagnostics.insert(PhaseCWindowDiagnosticV1 {
                pool_id: path.pool_id,
                code,
            });
        }
    }
    for attempt in admitted_attempts
        .iter()
        .filter(|attempt| window.contains(attempt.admitted_at_second))
    {
        if !expected.contains(&attempt.path) {
            diagnostics.insert(PhaseCWindowDiagnosticV1 {
                pool_id: 0,
                code: PhaseCWindowDiagnosticCodeV1::UnknownAttemptPath,
            });
            continue;
        }
        let pool_id = attempt.path.pool_id;
        for (invalid, code) in [
            (
                !attempt.has_authoritative_usage,
                PhaseCWindowDiagnosticCodeV1::MissingUsage,
            ),
            (
                attempt.lease_expired,
                PhaseCWindowDiagnosticCodeV1::ExpiredLease,
            ),
            (
                attempt.breaker_open,
                PhaseCWindowDiagnosticCodeV1::OpenBreaker,
            ),
        ] {
            if invalid {
                diagnostics.insert(PhaseCWindowDiagnosticV1 { pool_id, code });
            }
        }
        validate_attempt_chain(window, attempt, &mut diagnostics);
    }
    PhaseCWindowQualificationV1 {
        admitted: diagnostics.is_empty(),
        diagnostics: diagnostics.into_iter().collect(),
    }
}

fn validate_attempt_chain(
    window: AlignedPhaseCWindowV1,
    attempt: &PhaseCAdmittedAttemptV1,
    diagnostics: &mut BTreeSet<PhaseCWindowDiagnosticV1>,
) {
    let required = [
        PhaseCAttemptStageV1::Decision,
        PhaseCAttemptStageV1::Dispatch,
        PhaseCAttemptStageV1::Heartbeat,
        PhaseCAttemptStageV1::ProviderOutcome,
        PhaseCAttemptStageV1::Reconcile,
    ];
    let mut timestamps = Vec::with_capacity(required.len());
    for stage in required {
        let stages: Vec<_> = attempt
            .stages
            .iter()
            .filter(|item| item.stage == stage)
            .collect();
        let code = match stages.as_slice() {
            [] => Some(PhaseCWindowDiagnosticCodeV1::MissingStage),
            [item] if matches!(item.outcome, PhaseCAttemptEvidenceOutcomeV1::Missing) => {
                Some(PhaseCWindowDiagnosticCodeV1::MissingStageOutcome)
            }
            [item]
                if matches!(stage, PhaseCAttemptStageV1::ProviderOutcome)
                    != matches!(item.outcome, PhaseCAttemptEvidenceOutcomeV1::Provider(_)) =>
            {
                Some(PhaseCWindowDiagnosticCodeV1::InvalidStageOutcome)
            }
            [item] if !window.contains(item.timestamp_second) => {
                Some(PhaseCWindowDiagnosticCodeV1::StageOutsideWindow)
            }
            [_] => None,
            _ => Some(PhaseCWindowDiagnosticCodeV1::DuplicateStage),
        };
        if let Some(code) = code {
            diagnostics.insert(PhaseCWindowDiagnosticV1 {
                pool_id: attempt.path.pool_id,
                code,
            });
        } else if let [item] = stages.as_slice() {
            timestamps.push(item.timestamp_second);
        }
    }
    if timestamps.len() == required.len() && timestamps.windows(2).any(|pair| pair[0] >= pair[1]) {
        diagnostics.insert(PhaseCWindowDiagnosticV1 {
            pool_id: attempt.path.pool_id,
            code: PhaseCWindowDiagnosticCodeV1::ReversedStages,
        });
    }
}

async fn resolve_planned_routes(
    repository: &ModelTurnAdmissionRepository,
    catalog: &CatalogService,
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
        // Durable pool rows are identity, not a label authority. Require the
        // exact resolved pair to be catalog-bounded before it can enter the
        // coordinator-owned expected-path denominator.
        if catalog
            .find_model(&format!("{}/{}", pool.provider_id, pool.model_id))
            .is_none()
        {
            continue;
        }
        routes.push(pool);
    }
    routes.sort_by_key(|pool| pool.id);
    routes.dedup_by_key(|pool| pool.id);
    Ok(routes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_db::{
        Database, ModelTurnBucketDebit, ModelTurnBucketKind,
        repositories::test_support::seed_scoped_model_turn_admission_fixture,
    };
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
        seed_scoped_model_turn_admission_fixture(
            db,
            credential,
            provider,
            model,
            "shadow",
            "supported",
            1,
        )
        .await
    }
    #[tokio::test]
    async fn denominator_comes_only_from_live_slots_and_resolved_routes() {
        let db = Database::ephemeral().await.expect("db");
        let pool_id = seed(&db, "credential-a", "zai", "glm-5").await;
        let projection = project_from_inventory(
            &ModelTurnAdmissionRepository::new(db),
            &CatalogService::new(),
            vec![
                record(Some("ready-old"), Some("rev-1"), true, false),
                record(Some("ready-new"), Some("rev-2"), true, false),
                record(Some("not-ready"), Some("rev-3"), false, false),
                record(Some("terminal"), Some("rev-4"), true, true),
                record(None, Some("rev-5"), true, false),
                record(Some("missing-revision"), None, true, false),
            ],
            &[
                plan("credential-a", "zai", "glm-5"),
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
    async fn arbitrary_resolved_pool_labels_never_enter_the_denominator() {
        let db = Database::ephemeral().await.expect("db");
        seed(
            &db,
            "credential-arbitrary",
            "arbitrary-provider",
            "arbitrary-model",
        )
        .await;
        let projection = project_from_inventory(
            &ModelTurnAdmissionRepository::new(db),
            &CatalogService::new(),
            vec![record(Some("slot-a"), Some("rev-1"), true, false)],
            &[plan(
                "credential-arbitrary",
                "arbitrary-provider",
                "arbitrary-model",
            )],
            &[],
        )
        .await
        .expect("projection");
        assert!(projection.expected_paths.is_empty());
    }

    #[tokio::test]
    async fn reports_preserve_coverage_and_persist_complete_evidence_surface() {
        let db = Database::ephemeral().await.expect("db");
        let pool_id = seed(&db, "credential-a", "zai", "glm-5").await;
        let repository = ModelTurnAdmissionRepository::new(db);
        let report = ModelTurnCapabilityReportV2 {
            slot_pod_uid: "slot-a".into(),
            deployment_revision: "rev-1".into(),
            provider: "zai".into(),
            model_scope: "glm-5".into(),
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
            &CatalogService::new(),
            vec![record(Some("slot-a"), Some("rev-1"), true, false)],
            &[plan("credential-a", "zai", "glm-5")],
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
            row.stage == "heartbeat" && row.outcome == "recorded" && row.provider_id == "zai"
        }));
        assert!(evidence.iter().any(|row| {
            row.stage == "heartbeat" && row.outcome == "missing" && row.model_id == "glm-5"
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
        let first_pool_id = seed(&db, "credential-a", "zai", "glm-5").await;
        let second_pool_id = seed(&db, "credential-b", "zai", "glm-5").await;
        let repository = ModelTurnAdmissionRepository::new(db);
        let projection = project_from_inventory(
            &repository,
            &CatalogService::new(),
            vec![record(Some("slot-a"), Some("rev-1"), true, false)],
            &[
                plan("credential-a", "zai", "glm-5"),
                plan("credential-b", "zai", "glm-5"),
            ],
            &[ModelTurnCapabilityReportV2 {
                slot_pod_uid: "slot-a".into(),
                deployment_revision: "rev-1".into(),
                provider: "zai".into(),
                model_scope: "glm-5".into(),
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

    fn window() -> AlignedPhaseCWindowV1 {
        AlignedPhaseCWindowV1::new(120).expect("window")
    }
    fn capability(path: ExpectedAttemptPathV1) -> PhaseCCapabilityEvidenceV1 {
        PhaseCCapabilityEvidenceV1 {
            path,
            coverage_start_second: 100,
            coverage_end_second: 200,
            observed_at_second: 150,
            covered: true,
        }
    }
    fn attempt(path: ExpectedAttemptPathV1) -> PhaseCAdmittedAttemptV1 {
        let provider = ProviderOutcomeV1 {
            terminal: ProviderAttemptTerminalV1::Completed,
            authoritative_usage: None,
            observation: None,
            abort: ProviderAttemptAbortResultV1::NotRequested,
            token_emission: Default::default(),
        };
        let stages = [
            (PhaseCAttemptStageV1::Decision, 121),
            (PhaseCAttemptStageV1::Dispatch, 122),
            (PhaseCAttemptStageV1::Heartbeat, 123),
            (PhaseCAttemptStageV1::ProviderOutcome, 124),
            (PhaseCAttemptStageV1::Reconcile, 125),
        ]
        .into_iter()
        .map(|(stage, timestamp_second)| PhaseCAttemptStageEvidenceV1 {
            stage,
            timestamp_second,
            outcome: if stage == PhaseCAttemptStageV1::ProviderOutcome {
                PhaseCAttemptEvidenceOutcomeV1::Provider(Box::new(provider.clone()))
            } else {
                PhaseCAttemptEvidenceOutcomeV1::Recorded
            },
        })
        .collect();
        PhaseCAdmittedAttemptV1 {
            path,
            admitted_at_second: 120,
            has_authoritative_usage: true,
            lease_expired: false,
            breaker_open: false,
            stages,
        }
    }
    fn has(r: &PhaseCWindowQualificationV1, c: PhaseCWindowDiagnosticCodeV1) -> bool {
        r.diagnostics.iter().any(|d| d.code == c)
    }

    #[test]
    fn qualifier_accepts_crossing_evidence_and_exact_half_open_boundaries() {
        let path = ExpectedAttemptPathV1::test_resolved_route(7);
        let mut a = attempt(path.clone());
        a.stages[0].timestamp_second = 120;
        a.stages[4].timestamp_second = 179;
        assert!(
            qualify_aligned_phase_c_window_v1(
                window(),
                std::slice::from_ref(&path),
                &[capability(path.clone())],
                &[a],
            )
                .admitted
        );
        assert_eq!(window().end_second(), 180);
        assert_eq!(
            AlignedPhaseCWindowV1::new(121),
            Err(AlignedPhaseCWindowErrorV1::UnalignedStart)
        );
        assert_eq!(
            AlignedPhaseCWindowV1::new(9_223_372_036_854_775_800),
            Err(AlignedPhaseCWindowErrorV1::EndOverflow)
        );
        let mut end = attempt(ExpectedAttemptPathV1::test_resolved_route(7));
        end.admitted_at_second = 180;
        assert!(
            qualify_aligned_phase_c_window_v1(
                window(),
                &[ExpectedAttemptPathV1::test_resolved_route(7)],
                &[capability(ExpectedAttemptPathV1::test_resolved_route(7))],
                &[end]
            )
            .admitted
        );
    }
    #[test]
    fn qualifier_rejects_silent_stale_replaced_and_partial_capability() {
        let path = ExpectedAttemptPathV1::test_resolved_route(7);
        assert!(has(
            &qualify_aligned_phase_c_window_v1(window(), std::slice::from_ref(&path), &[], &[]),
            PhaseCWindowDiagnosticCodeV1::MissingCapability
        ));
        let stale = PhaseCCapabilityEvidenceV1 {
            observed_at_second: 119,
            ..capability(path.clone())
        };
        assert!(has(
            &qualify_aligned_phase_c_window_v1(
                window(),
                std::slice::from_ref(&path),
                &[stale],
                &[],
            ),
            PhaseCWindowDiagnosticCodeV1::StaleHeartbeat
        ));
        let partial = PhaseCCapabilityEvidenceV1 {
            coverage_end_second: 179,
            ..capability(path.clone())
        };
        assert!(has(
            &qualify_aligned_phase_c_window_v1(
                window(),
                std::slice::from_ref(&path),
                &[partial],
                &[],
            ),
            PhaseCWindowDiagnosticCodeV1::PartialCapabilityCoverage
        ));
        assert!(has(
            &qualify_aligned_phase_c_window_v1(
                window(),
                &[path],
                &[capability(ExpectedAttemptPathV1 {
                    deployment_revision: "replacement-revision".into(),
                    ..ExpectedAttemptPathV1::test_resolved_route(7)
                })],
                &[]
            ),
            PhaseCWindowDiagnosticCodeV1::UnexpectedCapability
        ));
    }
    #[test]
    fn qualifier_rejects_chain_and_operational_failures_and_redacts_diagnostics() {
        let path = ExpectedAttemptPathV1::test_resolved_route(7);
        let mut a = attempt(path.clone());
        a.has_authoritative_usage = false;
        a.lease_expired = true;
        a.breaker_open = true;
        a.stages[3].outcome = PhaseCAttemptEvidenceOutcomeV1::Recorded;
        a.stages.push(a.stages[0].clone());
        let r = qualify_aligned_phase_c_window_v1(
            window(),
            std::slice::from_ref(&path),
            &[capability(path.clone())],
            &[a],
        );
        for c in [
            PhaseCWindowDiagnosticCodeV1::MissingUsage,
            PhaseCWindowDiagnosticCodeV1::ExpiredLease,
            PhaseCWindowDiagnosticCodeV1::OpenBreaker,
            PhaseCWindowDiagnosticCodeV1::DuplicateStage,
            PhaseCWindowDiagnosticCodeV1::InvalidStageOutcome,
        ] {
            assert!(has(&r, c), "{c:?}");
        }
        let serialized = serde_json::to_value(&r).expect("serialize");
        assert_eq!(
            serialized,
            serde_json::json!({
                "admitted": false,
                "diagnostics": [
                    {"pool_id": 7, "code": "missing_usage"},
                    {"pool_id": 7, "code": "expired_lease"},
                    {"pool_id": 7, "code": "open_breaker"},
                    {"pool_id": 7, "code": "duplicate_stage"},
                    {"pool_id": 7, "code": "invalid_stage_outcome"}
                ]
            }),
            "the closed diagnostic schema has no surface for reporter text or identifiers"
        );
        let json = serialized.to_string();
        for value in [
            "slot-catalog-bound",
            "revision-catalog-bound",
            "attempt-fingerprint",
            "credential",
            "request-id",
            "lease-id",
            "reporter-text",
            "provider-catalog-bound",
            "model-catalog-bound",
            "arbitrary-label",
        ] {
            assert!(!json.contains(value), "leaked {value}");
        }
    }
    #[test]
    fn qualifier_rejects_missing_stage_outcome_and_partial_chain() {
        let path = ExpectedAttemptPathV1::test_resolved_route(7);
        let mut a = attempt(path.clone());
        a.stages[0].outcome = PhaseCAttemptEvidenceOutcomeV1::Missing;
        a.stages.pop();
        let r = qualify_aligned_phase_c_window_v1(
            window(),
            std::slice::from_ref(&path),
            &[capability(path.clone())],
            &[a],
        );
        assert!(has(&r, PhaseCWindowDiagnosticCodeV1::MissingStageOutcome));
        assert!(has(&r, PhaseCWindowDiagnosticCodeV1::MissingStage));
    }

    #[test]
    fn qualifier_rejects_unknown_paths_reversed_stages_and_exact_end_stages() {
        let path = ExpectedAttemptPathV1::test_resolved_route(7);
        let unknown = attempt(ExpectedAttemptPathV1::test_resolved_route(8));
        let mut reversed = attempt(path.clone());
        reversed.stages[2].timestamp_second = 122;
        let mut end = attempt(path.clone());
        end.stages[4].timestamp_second = 180;
        let r = qualify_aligned_phase_c_window_v1(
            window(),
            std::slice::from_ref(&path),
            &[capability(path.clone())],
            &[unknown, reversed, end],
        );
        for code in [
            PhaseCWindowDiagnosticCodeV1::UnknownAttemptPath,
            PhaseCWindowDiagnosticCodeV1::ReversedStages,
            PhaseCWindowDiagnosticCodeV1::StageOutsideWindow,
        ] {
            assert!(has(&r, code), "{code:?}");
        }
        assert!(!r.admitted);
    }
}
