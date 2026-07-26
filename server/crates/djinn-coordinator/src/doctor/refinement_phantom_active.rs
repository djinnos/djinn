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
        RefinementBetweenPhaseSnapshot, RefinementHeartbeatSnapshot, RefinementIntentSnapshot,
        RefinementIntentState, RefinementParkKind, RefinementParkSnapshot, RefinementPhase,
        RefinementRole, RefinementRunSnapshot, RefinementRunState, RefinementSessionEvidence,
        RefinementSessionState, RefinementTaskEvidence, RefinementTaskState,
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

    fn pending_intent(run_id: &str) -> RefinementIntentSnapshot {
        RefinementIntentSnapshot {
            intent_id: format!("intent-{run_id}"),
            run_id: run_id.into(),
            round: 1,
            state: RefinementIntentState::Pending,
            phase: RefinementPhase::AdversaryAttack,
            role: RefinementRole::Adversary,
            lease_expires_at: None,
        }
    }

    #[test]
    fn complete_live_evidence_matrix_produces_no_findings() {
        let mut pending = stale("run-pending");
        pending.snapshot.intents.push(pending_intent("run-pending"));
        let mut open = stale("run-open");
        open.snapshot.tasks.push(RefinementTaskEvidence {
            task_id: "open-task".into(),
            run_id: "run-open".into(),
            intent_id: None,
            state: RefinementTaskState::Open,
        });
        let mut queued = stale("run-queued");
        queued.snapshot.tasks.push(RefinementTaskEvidence {
            task_id: "queued-task".into(),
            run_id: "run-queued".into(),
            intent_id: None,
            state: RefinementTaskState::Queued,
        });
        let mut session = stale("run-session");
        session.snapshot.tasks.push(RefinementTaskEvidence {
            task_id: "session-task".into(),
            run_id: "run-session".into(),
            intent_id: None,
            state: RefinementTaskState::Closed,
        });
        session.snapshot.sessions.push(RefinementSessionEvidence {
            session_id: "live-session".into(),
            task_id: "session-task".into(),
            run_id: "run-session".into(),
            state: RefinementSessionState::Live,
        });
        let mut handoff = stale("run-handoff");
        handoff.snapshot.between_phase = Some(RefinementBetweenPhaseSnapshot {
            run_id: "run-handoff".into(),
            next_intent: pending_intent("run-handoff"),
        });
        let mut review = stale("run-review");
        review.snapshot.park = Some(RefinementParkSnapshot {
            run_id: "run-review".into(),
            kind: RefinementParkKind::AwaitingReview,
        });
        let mut evidence = stale("run-evidence");
        evidence.snapshot.park = Some(RefinementParkSnapshot {
            run_id: "run-evidence".into(),
            kind: RefinementParkKind::AwaitingEvidence,
        });
        let check =
            RefinementPhantomActiveCheck::new(Arc::new(MemoryRefinementPhantomActiveSource::new(
                vec![pending, open, queued, session, handoff, review, evidence],
            )));
        assert!(check.run().unwrap().is_empty());
        let clean = RefinementPhantomActiveCheck::new(Arc::new(
            MemoryRefinementPhantomActiveSource::new(vec![]),
        ));
        assert!(clean.run().unwrap().is_empty(), "clean/no-run fixture");
    }

    #[test]
    fn each_non_stale_fixture_is_suppressed_by_the_doctor_check() {
        let mut live_session = stale("run-live-session");
        live_session.snapshot.tasks.push(RefinementTaskEvidence {
            task_id: "session-task".into(),
            run_id: "run-live-session".into(),
            intent_id: None,
            state: RefinementTaskState::Closed,
        });
        live_session
            .snapshot
            .sessions
            .push(RefinementSessionEvidence {
                session_id: "session-live".into(),
                task_id: "session-task".into(),
                run_id: "run-live-session".into(),
                state: RefinementSessionState::Live,
            });
        let mut queued_task = stale("run-queued-task");
        queued_task.snapshot.tasks.push(RefinementTaskEvidence {
            task_id: "queued-task".into(),
            run_id: "run-queued-task".into(),
            intent_id: None,
            state: RefinementTaskState::Queued,
        });
        let mut open_task = stale("run-open-task");
        open_task.snapshot.tasks.push(RefinementTaskEvidence {
            task_id: "open-task".into(),
            run_id: "run-open-task".into(),
            intent_id: None,
            state: RefinementTaskState::Open,
        });
        let mut between_phase = stale("run-between-phase");
        between_phase.snapshot.between_phase = Some(RefinementBetweenPhaseSnapshot {
            run_id: "run-between-phase".into(),
            next_intent: pending_intent("run-between-phase"),
        });
        let mut awaiting_review = stale("run-awaiting-review");
        awaiting_review.snapshot.park = Some(RefinementParkSnapshot {
            run_id: "run-awaiting-review".into(),
            kind: RefinementParkKind::AwaitingReview,
        });
        let mut awaiting_evidence = stale("run-awaiting-evidence");
        awaiting_evidence.snapshot.park = Some(RefinementParkSnapshot {
            run_id: "run-awaiting-evidence".into(),
            kind: RefinementParkKind::AwaitingEvidence,
        });
        let fixtures = vec![
            ("clean/no-run", None),
            ("live session", Some(live_session)),
            ("queued task", Some(queued_task)),
            ("open task", Some(open_task)),
            ("between phase", Some(between_phase)),
            ("awaiting review", Some(awaiting_review)),
            ("awaiting evidence", Some(awaiting_evidence)),
        ];
        for (name, fixture) in fixtures {
            let check = RefinementPhantomActiveCheck::new(Arc::new(
                MemoryRefinementPhantomActiveSource::new(fixture.into_iter().collect()),
            ));
            assert!(
                check.run().expect("run doctor check").is_empty(),
                "{name} must be live"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn repository_backed_check_is_repeatedly_read_only_and_never_reaps() {
        use crate::refinement_dispatch::refinement_cap_tests::seed_refinement_fixture;
        use djinn_core::events::{DjinnEventEnvelope, EventBus};
        use djinn_db::{
            AdmitRefinementRunRequest, ProposalRepository, RefinementAdmissionOutcome,
            RefinementAdmissionSource,
        };

        let db = crate::test_helpers::create_test_db();
        let fixture = seed_refinement_fixture(&db).await;
        let repository = ProposalRepository::new(db.clone(), EventBus::noop());
        let (run_id, generation) = match repository
            .admit_refinement_run(AdmitRefinementRunRequest {
                proposal_id: fixture.proposal_id.clone(),
                idempotency_key: "doctor-read-only".into(),
                source: RefinementAdmissionSource::Demand {
                    demand_id: "doctor-read-only".into(),
                },
                heartbeat_grace_millis: HEARTBEAT_GRACE_MILLIS,
            })
            .await
            .expect("admit stale fixture")
        {
            RefinementAdmissionOutcome::Admitted {
                run_id, generation, ..
            }
            | RefinementAdmissionOutcome::Existing {
                run_id, generation, ..
            } => (run_id, generation),
        };
        djinn_db::test_support::make_refinement_run_phantom_for_test(&db, &run_id).await;
        let (events_tx, _events_rx) = tokio::sync::broadcast::channel::<DjinnEventEnvelope>(16);
        let check = RefinementPhantomActiveCheck::new(Arc::new(
            ProposalRepositoryRefinementPhantomActiveSource::new(db.clone(), events_tx),
        ));
        let audit_before =
            djinn_db::test_support::refinement_run_audit_for_test(&db, &run_id).await;
        let heartbeat_before: String =
            sqlx::query_scalar("SELECT heartbeat_at FROM refinement_runs WHERE id = $1")
                .bind(&run_id)
                .fetch_one(db.pool())
                .await
                .expect("read heartbeat");
        let lifecycle_before: Vec<(String, String, Option<String>, Option<serde_json::Value>)> =
            sqlx::query_as(
                "SELECT id, event_kind, refinement_stop_tag, refinement_stop_context \
                 FROM proposal_revisions WHERE proposal_id = $1 AND refinement_run_id = $2 \
                 ORDER BY id",
            )
            .bind(&fixture.proposal_id)
            .bind(&run_id)
            .fetch_all(db.pool())
            .await
            .expect("read lifecycle rows");
        let reaps_before: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM proposal_revisions \
             WHERE proposal_id = $1 AND refinement_stop_tag = 'reaped_phantom'",
        )
        .bind(&fixture.proposal_id)
        .fetch_one(db.pool())
        .await
        .expect("count durable phantom reaps");
        for _ in 0..2 {
            let findings = check.run().expect("run repository-backed doctor check");
            assert_eq!(findings.len(), 1);
            assert_eq!(findings[0].evidence["run_id"], run_id);
            assert_eq!(findings[0].evidence["generation"], generation);
        }
        assert_eq!(
            djinn_db::test_support::refinement_run_audit_for_test(&db, &run_id).await,
            audit_before,
            "run state and reap counter remain unchanged"
        );
        let heartbeat_after: String =
            sqlx::query_scalar("SELECT heartbeat_at FROM refinement_runs WHERE id = $1")
                .bind(&run_id)
                .fetch_one(db.pool())
                .await
                .expect("read heartbeat");
        let lifecycle_after: Vec<(String, String, Option<String>, Option<serde_json::Value>)> =
            sqlx::query_as(
                "SELECT id, event_kind, refinement_stop_tag, refinement_stop_context \
                 FROM proposal_revisions WHERE proposal_id = $1 AND refinement_run_id = $2 \
                 ORDER BY id",
            )
            .bind(&fixture.proposal_id)
            .bind(&run_id)
            .fetch_all(db.pool())
            .await
            .expect("read lifecycle rows");
        let reaps_after: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM proposal_revisions \
             WHERE proposal_id = $1 AND refinement_stop_tag = 'reaped_phantom'",
        )
        .bind(&fixture.proposal_id)
        .fetch_one(db.pool())
        .await
        .expect("count durable phantom reaps");
        assert_eq!(
            heartbeat_after, heartbeat_before,
            "doctor must not touch heartbeat"
        );
        assert_eq!(
            lifecycle_after, lifecycle_before,
            "doctor must not write or alter lifecycle/audit rows"
        );
        assert_eq!(
            reaps_after, reaps_before,
            "doctor must not increment reap counters"
        );
    }
}
