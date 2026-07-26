//! Read-only detector for active refinement runs with no liveness evidence.
//!
//! The source loads every nonterminal run as an exact repository snapshot. This
//! check only evaluates those observations; it never invokes recovery or reaping.

use std::sync::Arc;

use djinn_core::{
    doctor::{
        DoctorCheck, DoctorCheckCadence, DoctorResult, Finding, FindingSeverity, ResolverSnapshot,
    },
    refinement_liveness::{
        DbTimestamp, RefinementLivenessResult, RefinementLivenessSnapshot,
        evaluate_refinement_liveness,
    },
};
use serde_json::json;
use tracing::warn;

pub const REFINEMENT_PHANTOM_ACTIVE_CHECK_NAME: &str = "refinement_phantom_active";
const HEARTBEAT_GRACE_MILLIS: i64 = 60_000;
const MISSING_EVIDENCE_CLASSES: [&str; 6] = [
    "explicit_park",
    "pending_or_claimed_intent",
    "active_task",
    "live_session",
    "between_phase_handoff",
    "fresh_heartbeat",
];

/// One exact-run repository observation retained by the source cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefinementPhantomActiveSnapshot {
    pub proposal_id: String,
    pub generation: i32,
    pub snapshot: RefinementLivenessSnapshot,
    pub observed_at: DbTimestamp,
}

/// Read-only source for exact-run observations.
pub trait RefinementPhantomActiveSource: Send + Sync {
    fn snapshots(&self) -> Vec<RefinementPhantomActiveSnapshot>;
    fn refresh_for_run(&self) {}
}

/// Cheap, read-only stale-run check. Its default `fix` remains unsupported.
pub struct RefinementPhantomActiveCheck {
    source: Arc<dyn RefinementPhantomActiveSource>,
}

impl RefinementPhantomActiveCheck {
    pub fn new(source: Arc<dyn RefinementPhantomActiveSource>) -> Self {
        Self { source }
    }

    fn finding_for(observation: &RefinementPhantomActiveSnapshot) -> Option<Finding> {
        if !matches!(
            evaluate_refinement_liveness(&observation.snapshot, observation.observed_at),
            RefinementLivenessResult::Stale { .. }
        ) {
            return None;
        }
        let run_id = observation.snapshot.run.run_id.clone();
        let last_heartbeat = observation
            .snapshot
            .heartbeat
            .as_ref()
            .filter(|heartbeat| heartbeat.run_id == run_id)
            .map(|heartbeat| heartbeat.heartbeat_at.0);
        let evidence = json!({
            "proposal_id": observation.proposal_id,
            "run_id": run_id,
            "generation": observation.generation,
            "last_heartbeat": last_heartbeat,
            "missing_evidence_classes": MISSING_EVIDENCE_CLASSES,
        });
        Some(
            Finding::new(
                FindingSeverity::Warn,
                REFINEMENT_PHANTOM_ACTIVE_CHECK_NAME,
                ResolverSnapshot::new(
                    "evaluate_refinement_liveness",
                    json!({
                        "proposal_id": observation.proposal_id,
                        "run_id": observation.snapshot.run.run_id,
                        "generation": observation.generation,
                        "observed_at": observation.observed_at.0,
                    }),
                    json!({"liveness": "stale", "reason": "no_live_evidence"}),
                ),
                format!(
                    "active refinement run {} for proposal {} has no live exact-run evidence",
                    observation.snapshot.run.run_id, observation.proposal_id
                ),
            )
            .with_entity_id("proposal_id", observation.proposal_id.clone())
            .with_entity_id("run_id", observation.snapshot.run.run_id.clone())
            .with_evidence(evidence),
        )
    }
}

impl DoctorCheck for RefinementPhantomActiveCheck {
    fn name(&self) -> &'static str {
        REFINEMENT_PHANTOM_ACTIVE_CHECK_NAME
    }

    fn description(&self) -> &'static str {
        "Reports active refinement runs with no shared-evaluator liveness evidence; read-only"
    }

    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::Cheap
    }

    fn run(&self) -> DoctorResult<Vec<Finding>> {
        self.source.refresh_for_run();
        let mut snapshots = self.source.snapshots();
        snapshots.sort_by(|left, right| {
            (
                left.proposal_id.as_str(),
                left.snapshot.run.run_id.as_str(),
                left.generation,
            )
                .cmp(&(
                    right.proposal_id.as_str(),
                    right.snapshot.run.run_id.as_str(),
                    right.generation,
                ))
        });
        Ok(snapshots.iter().filter_map(Self::finding_for).collect())
    }
}

#[derive(Clone, Debug, Default)]
pub struct MemoryRefinementPhantomActiveSource {
    snapshots: Vec<RefinementPhantomActiveSnapshot>,
}

impl MemoryRefinementPhantomActiveSource {
    pub fn new(snapshots: Vec<RefinementPhantomActiveSnapshot>) -> Self {
        Self { snapshots }
    }
}

impl RefinementPhantomActiveSource for MemoryRefinementPhantomActiveSource {
    fn snapshots(&self) -> Vec<RefinementPhantomActiveSnapshot> {
        self.snapshots.clone()
    }
}

/// Repository-backed cache for the synchronous doctor trait.
#[derive(Clone)]
pub struct ProposalRepositoryRefinementPhantomActiveSource {
    db: djinn_db::Database,
    events_tx: tokio::sync::broadcast::Sender<djinn_core::events::DjinnEventEnvelope>,
    cache: Arc<tokio::sync::RwLock<Vec<RefinementPhantomActiveSnapshot>>>,
}

impl ProposalRepositoryRefinementPhantomActiveSource {
    pub fn new(
        db: djinn_db::Database,
        events_tx: tokio::sync::broadcast::Sender<djinn_core::events::DjinnEventEnvelope>,
    ) -> Self {
        Self {
            db,
            events_tx,
            cache: Arc::new(tokio::sync::RwLock::new(Vec::new())),
        }
    }

    pub async fn refresh(&self) {
        let repository = djinn_db::ProposalRepository::new(
            self.db.clone(),
            crate::events::event_bus_for(&self.events_tx),
        );
        let runs = match repository.load_recoverable_refinement_runs().await {
            Ok(runs) => runs,
            Err(error) => {
                warn!(%error, "refinement_phantom_active doctor: failed to enumerate active runs");
                return;
            }
        };
        let mut snapshots = Vec::with_capacity(runs.len());
        for run in runs {
            match repository
                .load_refinement_run_snapshot(djinn_db::LoadRefinementRunSnapshotRequest {
                    run_id: run.run_id,
                    heartbeat_grace_millis: HEARTBEAT_GRACE_MILLIS,
                })
                .await
            {
                Ok(Some(exact)) if exact.generation == run.generation => {
                    snapshots.push(RefinementPhantomActiveSnapshot {
                        proposal_id: exact.proposal_id,
                        generation: exact.generation,
                        snapshot: exact.snapshot,
                        observed_at: exact.observed_at,
                    })
                }
                Ok(_) => {}
                Err(error) => {
                    warn!(%error, "refinement_phantom_active doctor: failed exact-run observation")
                }
            }
        }
        *self.cache.write().await = snapshots;
    }

    fn refresh_blocking(&self) {
        let source = self.clone();
        match tokio::runtime::Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| handle.block_on(source.refresh()));
            }
            Ok(_) => {
                let _ = std::thread::spawn(move || match tokio::runtime::Runtime::new() {
                    Ok(runtime) => runtime.block_on(source.refresh()),
                    Err(error) => warn!(%error, "refinement_phantom_active doctor: failed to create refresh runtime"),
                }).join();
            }
            Err(_) => match tokio::runtime::Runtime::new() {
                Ok(runtime) => runtime.block_on(source.refresh()),
                Err(error) => {
                    warn!(%error, "refinement_phantom_active doctor: failed to create refresh runtime")
                }
            },
        }
    }
}

impl RefinementPhantomActiveSource for ProposalRepositoryRefinementPhantomActiveSource {
    fn snapshots(&self) -> Vec<RefinementPhantomActiveSnapshot> {
        self.cache
            .try_read()
            .map(|cache| cache.clone())
            .unwrap_or_default()
    }

    fn refresh_for_run(&self) {
        self.refresh_blocking();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_core::refinement_liveness::{
        RefinementHeartbeatSnapshot, RefinementIntentSnapshot, RefinementIntentState,
        RefinementPhase, RefinementRole, RefinementRunSnapshot, RefinementRunState,
    };

    fn stale(run_id: &str) -> RefinementPhantomActiveSnapshot {
        RefinementPhantomActiveSnapshot {
            proposal_id: "proposal-1".into(),
            generation: 7,
            observed_at: DbTimestamp(10_000),
            snapshot: RefinementLivenessSnapshot {
                run: RefinementRunSnapshot {
                    run_id: run_id.into(),
                    state: RefinementRunState::Active,
                    terminal_reason: None,
                },
                park: None,
                intents: vec![],
                tasks: vec![],
                sessions: vec![],
                between_phase: None,
                heartbeat: Some(RefinementHeartbeatSnapshot {
                    run_id: run_id.into(),
                    heartbeat_at: DbTimestamp(1),
                    grace_millis: 1,
                }),
            },
        }
    }

    #[test]
    fn stale_finding_is_bounded_deterministic_and_has_no_fix() {
        let check = RefinementPhantomActiveCheck::new(Arc::new(
            MemoryRefinementPhantomActiveSource::new(vec![stale("run-1")]),
        ));
        let findings = check.run().unwrap();
        assert_eq!(findings.len(), 1);
        assert_eq!(
            findings[0].evidence,
            json!({"proposal_id":"proposal-1","run_id":"run-1","generation":7,"last_heartbeat":1,"missing_evidence_classes":MISSING_EVIDENCE_CLASSES})
        );
        assert!(check.fix(&findings[0]).is_err());
    }

    #[test]
    fn evaluator_live_intent_produces_no_finding() {
        let mut live = stale("run-live");
        live.snapshot.intents.push(RefinementIntentSnapshot {
            intent_id: "intent".into(),
            run_id: "run-live".into(),
            round: 1,
            state: RefinementIntentState::Pending,
            phase: RefinementPhase::AdversaryAttack,
            role: RefinementRole::Adversary,
            lease_expires_at: None,
        });
        let check = RefinementPhantomActiveCheck::new(Arc::new(
            MemoryRefinementPhantomActiveSource::new(vec![live]),
        ));
        assert!(check.run().unwrap().is_empty());
    }
}
