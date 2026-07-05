//! Agent/coordinator-backed `zombie_running_session` doctor check.
//!
//! The check detects the canonical divergence used by the ed05 proof: the DB
//! says a non-chat `sessions` row is still `running` and belongs to a task, but
//! the coordinator's in-process liveness surfaces report no live slot/session
//! and no connected worker for that task. It is intentionally read-only; the
//! existing DB-truth zombie reaper owns mutation/recovery.
//!
//! This implementation aligns its finding evidence with the 5ric persisted
//! liveness classifier semantics. Raw slot/RPC/pod signals are still captured
//! for diagnosis, but the resolver output and finding evidence now surface the
//! same verdict/outcome/reason concepts that session recovery persists via
//! [`djinn_db::LivenessRepository`].

use std::sync::Arc;

use djinn_core::doctor::{
    DoctorCheck, DoctorCheckCadence, DoctorResult, Finding, FindingSeverity, ResolverSnapshot,
};
use djinn_core::models::SessionRecord;
use djinn_db::{CurrentLivenessState, LivenessRepository};
use djinn_supervisor::ConnectionRegistry;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::warn;

use crate::dispatch::liveness::{LivenessOutcome, Verdict};

use djinn_slot::SlotPoolHandle;

pub const ZOMBIE_RUNNING_SESSION_CHECK_NAME: &str = "zombie_running_session";

/// Read-only source for already-collected zombie-session candidates.
///
/// The doctor framework's `DoctorCheck::run` is synchronous, so production code
/// snapshots DB/slot/RPC/liveness state before constructing the check. Tests
/// can provide a pure in-memory source directly, keeping the resolver hermetic
/// and cluster-free.
pub trait ZombieRunningSessionSource: Send + Sync {
    fn candidates(&self) -> Vec<ZombieRunningSessionCandidate>;
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZombieRunningSessionCandidate {
    pub session_id: String,
    pub task_id: Option<String>,
    pub task_run_id: Option<String>,
    pub db_status: String,
    pub agent_type: String,
    pub started_at: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub slot_pool_has_session: bool,
    pub slot_pool_session_present: bool,
    pub worker_connected: bool,
    pub pod_present: bool,
    /// Task/session liveness state loaded from the persisted classifier.
    pub liveness_state: Option<ZombieRunningSessionLivenessState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZombieRunningSessionLivenessState {
    pub task_status: Option<String>,
    pub task_is_terminal: bool,
    pub active_session_status: Option<String>,
    pub liveness_verdict: Option<String>,
    pub liveness_outcome_kind: Option<String>,
    pub liveness_outcome_reason: Option<String>,
    pub liveness_evidence: Option<serde_json::Value>,
}

impl From<&CurrentLivenessState> for ZombieRunningSessionLivenessState {
    fn from(state: &CurrentLivenessState) -> Self {
        Self {
            task_status: state.task_status.clone(),
            task_is_terminal: state.task_is_terminal,
            active_session_status: state.active_session_status.clone(),
            liveness_verdict: state.session_liveness_verdict.clone(),
            liveness_outcome_kind: state.session_liveness_outcome_kind.clone(),
            liveness_outcome_reason: state.session_liveness_outcome_reason.clone(),
            liveness_evidence: state.session_liveness_evidence.clone(),
        }
    }
}

impl ZombieRunningSessionCandidate {
    fn from_session(
        session: &SessionRecord,
        slot_pool_has_session: bool,
        slot_pool_session_present: bool,
        worker_connected: bool,
        pod_present: bool,
        liveness_state: Option<ZombieRunningSessionLivenessState>,
    ) -> Self {
        Self {
            session_id: session.id.clone(),
            task_id: session.task_id.clone(),
            task_run_id: session.task_run_id.clone(),
            db_status: session.status.clone(),
            agent_type: session.agent_type.clone(),
            started_at: session.started_at.clone(),
            tokens_in: session.tokens_in,
            tokens_out: session.tokens_out,
            slot_pool_has_session,
            slot_pool_session_present,
            worker_connected,
            pod_present,
            liveness_state,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZombieRunningSessionInputs {
    pub session_id: String,
    pub task_id: Option<String>,
    pub task_run_id: Option<String>,
    pub db_status: String,
    pub agent_type: String,
    pub started_at: String,
    pub tokens_in: i64,
    pub tokens_out: i64,
    pub live_state: ZombieRunningSessionLiveState,
    pub liveness_state: Option<ZombieRunningSessionLivenessState>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ZombieRunningSessionLiveState {
    pub slot_pool_has_session: bool,
    pub slot_pool_session_present: bool,
    pub worker_connected: bool,
    pub pod_present: bool,
}

/// Local reason for the resolver decision. This is **not** the persisted
/// liveness reason; it describes why the doctor check did or did not emit a
/// zombie finding. The classifier-aligned outcome/reason/evidence live in
/// [`ZombieRunningSessionOutputs`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ZombieRunningSessionReason {
    ClassifierDead,
    NotRunning,
    ChatSession,
    NoTaskId,
    TerminalTask,
    KnownSuccessOutcome,
    LiveSlotOrSessionState,
    LiveWorkerConnection,
    LivePod,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ZombieRunningSessionOutputs {
    pub is_zombie: bool,
    pub reason: ZombieRunningSessionReason,
    /// Liveness verdict aligned with the 5ric classifier (e.g. `dead`).
    pub liveness_verdict: Option<String>,
    /// Liveness outcome kind aligned with the 5ric classifier (e.g.
    /// `dead_reclaimed`).
    pub liveness_outcome: Option<String>,
    /// Liveness reason aligned with the 5ric classifier (e.g.
    /// `nonzero_exit_nonterminal`).
    pub liveness_reason: Option<String>,
}

fn resolve_zombie_running_session(
    inputs: &ZombieRunningSessionInputs,
) -> ZombieRunningSessionOutputs {
    // Helper: echo persisted classifier fields into the output.
    let persisted = |is_zombie: bool, reason: ZombieRunningSessionReason| {
        ZombieRunningSessionOutputs {
            is_zombie,
            reason,
            liveness_verdict: inputs.liveness_state.as_ref().and_then(|s| s.liveness_verdict.clone()),
            liveness_outcome: inputs
                .liveness_state
                .as_ref()
                .and_then(|s| s.liveness_outcome_kind.clone()),
            liveness_reason: inputs
                .liveness_state
                .as_ref()
                .and_then(|s| s.liveness_outcome_reason.clone()),
        }
    };

    if inputs.db_status != "running" {
        return persisted(false, ZombieRunningSessionReason::NotRunning);
    }
    if inputs.agent_type == "chat" {
        return persisted(false, ZombieRunningSessionReason::ChatSession);
    }
    if inputs.task_id.is_none() {
        return persisted(false, ZombieRunningSessionReason::NoTaskId);
    }

    // Terminal task state wins over raw-signal zombie detection: a task that
    // has already finished should not produce a misleading zombie finding.
    if inputs
        .liveness_state
        .as_ref()
        .is_some_and(|s| s.task_is_terminal)
    {
        return persisted(false, ZombieRunningSessionReason::TerminalTask);
    }

    // Persisted non-zombie success/noop outcomes suppress the finding: the
    // classifier has already resolved this session to a known-good terminal
    // state. This avoids duplicating the reaper's verdict.
    if let Some(ZombieRunningSessionLivenessState {
        liveness_outcome_kind: Some(ref outcome),
        ..
    }) = inputs.liveness_state
    {
        match outcome.as_str() {
            "success" | "kill_noop" | "slow_extended" => {
                return persisted(false, ZombieRunningSessionReason::KnownSuccessOutcome);
            }
            _ => {}
        }
    }

    if inputs.live_state.slot_pool_has_session || inputs.live_state.slot_pool_session_present {
        return persisted(false, ZombieRunningSessionReason::LiveSlotOrSessionState);
    }
    if inputs.live_state.worker_connected {
        return persisted(false, ZombieRunningSessionReason::LiveWorkerConnection);
    }
    if inputs.live_state.pod_present {
        return persisted(false, ZombieRunningSessionReason::LivePod);
    }

    // No live signals and no persisted non-zombie classifier outcome: report
    // a zombie finding using the 5ric classifier vocabulary. If the persisted
    // classifier already produced an outcome (e.g. `crash`), echo it rather
    // than overriding it with the generic `dead_reclaimed` default.
    ZombieRunningSessionOutputs {
        is_zombie: true,
        reason: ZombieRunningSessionReason::ClassifierDead,
        liveness_verdict: inputs
            .liveness_state
            .as_ref()
            .and_then(|s| s.liveness_verdict.clone())
            .or(Some(Verdict::Dead.as_str().to_owned())),
        liveness_outcome: inputs
            .liveness_state
            .as_ref()
            .and_then(|s| s.liveness_outcome_kind.clone())
            .or(Some(LivenessOutcome::DeadReclaimed.as_str().to_owned())),
        liveness_reason: inputs
            .liveness_state
            .as_ref()
            .and_then(|s| s.liveness_outcome_reason.clone()),
    }
}

/// Cheap, read-only doctor check for DB-running/no-live-session divergence.
pub struct ZombieRunningSessionCheck<S: ZombieRunningSessionSource> {
    source: S,
}

impl<S: ZombieRunningSessionSource> ZombieRunningSessionCheck<S> {
    pub fn new(source: S) -> Self {
        Self { source }
    }

    fn candidate_inputs(candidate: ZombieRunningSessionCandidate) -> ZombieRunningSessionInputs {
        ZombieRunningSessionInputs {
            session_id: candidate.session_id,
            task_id: candidate.task_id,
            task_run_id: candidate.task_run_id,
            db_status: candidate.db_status,
            agent_type: candidate.agent_type,
            started_at: candidate.started_at,
            tokens_in: candidate.tokens_in,
            tokens_out: candidate.tokens_out,
            live_state: ZombieRunningSessionLiveState {
                slot_pool_has_session: candidate.slot_pool_has_session,
                slot_pool_session_present: candidate.slot_pool_session_present,
                worker_connected: candidate.worker_connected,
                pod_present: candidate.pod_present,
            },
            liveness_state: candidate.liveness_state,
        }
    }

    fn finding_for(inputs: ZombieRunningSessionInputs) -> Option<Finding> {
        let outputs = resolve_zombie_running_session(&inputs);
        if !outputs.is_zombie {
            return None;
        }

        let inputs_json = match serde_json::to_value(&inputs) {
            Ok(value) => value,
            Err(e) => {
                warn!(
                    session_id = %inputs.session_id,
                    error = %e,
                    "zombie_running_session doctor: failed to serialize resolver inputs; skipping finding"
                );
                return None;
            }
        };
        let outputs_json = match serde_json::to_value(&outputs) {
            Ok(value) => value,
            Err(e) => {
                warn!(
                    session_id = %inputs.session_id,
                    error = %e,
                    "zombie_running_session doctor: failed to serialize resolver outputs; skipping finding"
                );
                return None;
            }
        };
        let snapshot = ResolverSnapshot::new(
            "resolve_zombie_running_session",
            inputs_json.clone(),
            outputs_json.clone(),
        );
        let task_id = inputs.task_id.as_deref().unwrap_or_default().to_owned();
        let evidence = json!({
            "db_row": {
                "session_id": inputs.session_id,
                "task_id": inputs.task_id,
                "task_run_id": inputs.task_run_id,
                "status": inputs.db_status,
                "agent_type": inputs.agent_type,
                "started_at": inputs.started_at,
                "tokens_in": inputs.tokens_in,
                "tokens_out": inputs.tokens_out,
            },
            "live_state": {
                "slot_pool_has_session": inputs.live_state.slot_pool_has_session,
                "slot_pool_session_present": inputs.live_state.slot_pool_session_present,
                "worker_connected": inputs.live_state.worker_connected,
                "pod_present": inputs.live_state.pod_present,
            },
            "classifier": {
                "verdict": outputs.liveness_verdict,
                "outcome": outputs.liveness_outcome,
                "reason": outputs.liveness_reason,
                "task_status": inputs.liveness_state.as_ref().and_then(|s| s.task_status.clone()),
                "task_is_terminal": inputs.liveness_state.as_ref().map(|s| s.task_is_terminal).unwrap_or(false),
                "active_session_status": inputs.liveness_state.as_ref().and_then(|s| s.active_session_status.clone()),
                "persisted_verdict": inputs.liveness_state.as_ref().and_then(|s| s.liveness_verdict.clone()),
                "persisted_outcome_kind": inputs.liveness_state.as_ref().and_then(|s| s.liveness_outcome_kind.clone()),
                "persisted_outcome_reason": inputs.liveness_state.as_ref().and_then(|s| s.liveness_outcome_reason.clone()),
            },
            "resolver_outputs": outputs_json,
        });

        Some(
            Finding::new(
                FindingSeverity::Critical,
                ZOMBIE_RUNNING_SESSION_CHECK_NAME,
                snapshot,
                format!(
                    "session {} for task {} is running in DB but has no live slot/session, worker connection, or pod",
                    inputs.session_id, task_id
                ),
            )
            .with_entity_id("session_id", inputs.session_id)
            .with_entity_id("task_id", task_id)
            .with_evidence(evidence),
        )
    }
}

impl<S: ZombieRunningSessionSource + Send + Sync> DoctorCheck for ZombieRunningSessionCheck<S> {
    fn name(&self) -> &'static str {
        ZOMBIE_RUNNING_SESSION_CHECK_NAME
    }

    fn description(&self) -> &'static str {
        "Flags DB-running task sessions whose coordinator slot/session, worker connection, and pod liveness are all missing, using 5ric classifier evidence"
    }

    fn cadence(&self) -> DoctorCheckCadence {
        DoctorCheckCadence::Cheap
    }

    fn run(&self) -> DoctorResult<Vec<Finding>> {
        Ok(self
            .source
            .candidates()
            .into_iter()
            .filter_map(|candidate| Self::finding_for(Self::candidate_inputs(candidate)))
            .collect())
    }
}

#[derive(Clone, Debug, Default)]
pub struct SnapshotZombieRunningSessionSource {
    candidates: Vec<ZombieRunningSessionCandidate>,
}

impl SnapshotZombieRunningSessionSource {
    pub fn new(candidates: Vec<ZombieRunningSessionCandidate>) -> Self {
        Self { candidates }
    }

    pub async fn from_coordinator_state(
        db: &djinn_db::Database,
        events_tx: &tokio::sync::broadcast::Sender<djinn_core::events::DjinnEventEnvelope>,
        pool: &SlotPoolHandle,
        rpc_registry: Option<Arc<ConnectionRegistry>>,
    ) -> djinn_db::Result<Self> {
        let session_repo =
            djinn_db::SessionRepository::new(db.clone(), crate::events::event_bus_for(events_tx));
        let liveness_repo = LivenessRepository::new(db.clone());
        let sessions = session_repo.list_active().await?;
        let mut candidates = Vec::new();

        for session in sessions {
            let liveness_state = match session.task_id.as_deref() {
                Some(task_id) => liveness_repo
                    .load_current_state(task_id)
                    .await
                    .ok()
                    .map(|s| ZombieRunningSessionLivenessState::from(&s)),
                None => None,
            };

            let Some(task_id) = session.task_id.as_deref() else {
                candidates.push(ZombieRunningSessionCandidate::from_session(
                    &session, false, false, false, false, liveness_state,
                ));
                continue;
            };

            let slot_pool_has_session = pool.has_session(task_id).await.unwrap_or(false);
            let slot_pool_session_present = pool
                .session_for_task(task_id)
                .await
                .ok()
                .flatten()
                .is_some();
            let worker_connected = match (rpc_registry.as_ref(), session.task_run_id.as_deref()) {
                (Some(registry), Some(task_run_id)) => registry.is_connected(task_run_id).await,
                _ => false,
            };
            let pod_present = false;

            candidates.push(ZombieRunningSessionCandidate::from_session(
                &session,
                slot_pool_has_session,
                slot_pool_session_present,
                worker_connected,
                pod_present,
                liveness_state,
            ));
        }

        Ok(Self { candidates })
    }
}

impl ZombieRunningSessionSource for SnapshotZombieRunningSessionSource {
    fn candidates(&self) -> Vec<ZombieRunningSessionCandidate> {
        self.candidates.clone()
    }
}

/// Build the concrete cheap `zombie_running_session` check from a coordinator
/// state snapshot.
///
/// This is the importable seam the coordinator leader-tick helper uses: it
/// collects the async DB/slot/RPC/liveness signals once, then returns a
/// synchronous [`DoctorCheck`] whose `run()` is hermetic and read-only. Pod
/// liveness is deliberately represented by the snapshotted candidate state so
/// tests can keep using the in-process coordinator fixture without a real k8s
/// cluster.
pub async fn check_from_coordinator_state(
    db: &djinn_db::Database,
    events_tx: &tokio::sync::broadcast::Sender<djinn_core::events::DjinnEventEnvelope>,
    pool: &SlotPoolHandle,
    rpc_registry: Option<Arc<ConnectionRegistry>>,
) -> djinn_db::Result<ZombieRunningSessionCheck<SnapshotZombieRunningSessionSource>> {
    let source = SnapshotZombieRunningSessionSource::from_coordinator_state(
        db,
        events_tx,
        pool,
        rpc_registry,
    )
    .await?;
    Ok(ZombieRunningSessionCheck::new(source))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn liveness_state_for_running_task() -> Option<ZombieRunningSessionLivenessState> {
        Some(ZombieRunningSessionLivenessState {
            task_status: Some("in_progress".to_owned()),
            task_is_terminal: false,
            active_session_status: Some("running".to_owned()),
            liveness_verdict: None,
            liveness_outcome_kind: None,
            liveness_outcome_reason: None,
            liveness_evidence: None,
        })
    }

    fn zombie_candidate() -> ZombieRunningSessionCandidate {
        ZombieRunningSessionCandidate {
            session_id: "session-zombie".to_owned(),
            task_id: Some("task-zombie".to_owned()),
            task_run_id: Some("run-zombie".to_owned()),
            db_status: "running".to_owned(),
            agent_type: "worker".to_owned(),
            started_at: "2026-01-02T03:04:05.000Z".to_owned(),
            tokens_in: 0,
            tokens_out: 0,
            slot_pool_has_session: false,
            slot_pool_session_present: false,
            worker_connected: false,
            pod_present: false,
            liveness_state: liveness_state_for_running_task(),
        }
    }

    fn run_one(candidate: ZombieRunningSessionCandidate) -> Vec<Finding> {
        ZombieRunningSessionCheck::new(SnapshotZombieRunningSessionSource::new(vec![candidate]))
            .run()
            .expect("check runs")
    }

    #[test]
    fn check_is_cheap_and_uses_stable_name() {
        let check = ZombieRunningSessionCheck::new(SnapshotZombieRunningSessionSource::default());
        assert_eq!(check.name(), ZOMBIE_RUNNING_SESSION_CHECK_NAME);
        assert_eq!(check.cadence(), DoctorCheckCadence::Cheap);
    }

    #[test]
    fn db_running_without_live_session_reports_critical_finding() {
        let findings = run_one(zombie_candidate());
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];

        assert_eq!(finding.check_name, ZOMBIE_RUNNING_SESSION_CHECK_NAME);
        assert_eq!(finding.severity, FindingSeverity::Critical);
        assert!(
            finding.evidence.is_object(),
            "zombie findings must carry structured evidence"
        );
        assert!(
            finding.resolver_snapshot.inputs.is_object(),
            "zombie findings must snapshot resolver inputs"
        );
        assert!(
            finding.resolver_snapshot.outputs.is_object(),
            "zombie findings must snapshot resolver outputs"
        );
        assert_eq!(
            finding.entity_ids.get("session_id").map(String::as_str),
            Some("session-zombie")
        );
        assert_eq!(
            finding.entity_ids.get("task_id").map(String::as_str),
            Some("task-zombie")
        );
        assert_eq!(finding.evidence["db_row"]["status"], "running");
        assert_eq!(finding.evidence["resolver_outputs"]["reason"], "classifier_dead");
        assert!(
            !finding.evidence["live_state"]["slot_pool_has_session"]
                .as_bool()
                .unwrap()
        );
        assert!(
            !finding.evidence["live_state"]["slot_pool_session_present"]
                .as_bool()
                .unwrap()
        );
        assert!(
            !finding.evidence["live_state"]["worker_connected"]
                .as_bool()
                .unwrap()
        );
        assert!(
            !finding.evidence["live_state"]["pod_present"]
                .as_bool()
                .unwrap()
        );
        // Classifier-aligned evidence is present and uses 5ric vocabulary.
        assert_eq!(finding.evidence["classifier"]["verdict"], "dead");
        assert_eq!(finding.evidence["classifier"]["outcome"], "dead_reclaimed");
        assert_eq!(
            finding.resolver_snapshot.resolver,
            "resolve_zombie_running_session"
        );
        assert_eq!(finding.resolver_snapshot.inputs["db_status"], "running");
        assert_eq!(
            finding.resolver_snapshot.inputs["live_state"]["slot_pool_has_session"],
            false
        );
        assert_eq!(
            finding.resolver_snapshot.inputs["live_state"]["slot_pool_session_present"],
            false
        );
        assert_eq!(
            finding.resolver_snapshot.inputs["live_state"]["worker_connected"],
            false
        );
        assert_eq!(
            finding.resolver_snapshot.inputs["live_state"]["pod_present"],
            false
        );
        assert_eq!(finding.resolver_snapshot.outputs["reason"], "classifier_dead");
        assert_eq!(finding.resolver_snapshot.outputs["liveness_verdict"], "dead");
        assert_eq!(
            finding.resolver_snapshot.outputs["liveness_outcome"],
            "dead_reclaimed"
        );
    }

    #[test]
    fn live_slot_or_worker_suppresses_finding() {
        let mut candidate = zombie_candidate();
        candidate.slot_pool_has_session = true;
        assert!(run_one(candidate).is_empty());

        let mut candidate = zombie_candidate();
        candidate.slot_pool_session_present = true;
        assert!(run_one(candidate).is_empty());

        let mut candidate = zombie_candidate();
        candidate.worker_connected = true;
        assert!(run_one(candidate).is_empty());

        let mut candidate = zombie_candidate();
        candidate.pod_present = true;
        assert!(run_one(candidate).is_empty());
    }

    #[test]
    fn non_running_or_taskless_rows_do_not_report_findings() {
        let mut candidate = zombie_candidate();
        candidate.db_status = "completed".to_owned();
        assert!(run_one(candidate).is_empty());

        let mut candidate = zombie_candidate();
        candidate.task_id = None;
        assert!(run_one(candidate).is_empty());
    }

    #[test]
    fn terminal_task_suppresses_finding() {
        let mut candidate = zombie_candidate();
        candidate.liveness_state = Some(ZombieRunningSessionLivenessState {
            task_status: Some("closed".to_owned()),
            task_is_terminal: true,
            active_session_status: Some("running".to_owned()),
            liveness_verdict: None,
            liveness_outcome_kind: None,
            liveness_outcome_reason: None,
            liveness_evidence: None,
        });
        assert!(run_one(candidate).is_empty());
    }

    #[test]
    fn persisted_success_outcome_suppresses_finding() {
        let mut candidate = zombie_candidate();
        candidate.liveness_state = Some(ZombieRunningSessionLivenessState {
            task_status: Some("in_progress".to_owned()),
            task_is_terminal: false,
            active_session_status: Some("running".to_owned()),
            liveness_verdict: Some("live".to_owned()),
            liveness_outcome_kind: Some("success".to_owned()),
            liveness_outcome_reason: Some("clean_exit_nonterminal".to_owned()),
            liveness_evidence: Some(json!({"exit_code": 0})),
        });
        assert!(run_one(candidate).is_empty());
    }

    #[test]
    fn persisted_classifier_evidence_is_echoed_in_finding() {
        let mut candidate = zombie_candidate();
        candidate.liveness_state = Some(ZombieRunningSessionLivenessState {
            task_status: Some("in_progress".to_owned()),
            task_is_terminal: false,
            active_session_status: Some("running".to_owned()),
            liveness_verdict: Some("dead".to_owned()),
            liveness_outcome_kind: Some("crash".to_owned()),
            liveness_outcome_reason: Some("nonzero_exit_nonterminal".to_owned()),
            liveness_evidence: Some(json!({"exit_code": 137})),
        });
        let findings = run_one(candidate);
        assert_eq!(findings.len(), 1);
        let finding = &findings[0];
        assert_eq!(finding.evidence["classifier"]["verdict"], "dead");
        assert_eq!(finding.evidence["classifier"]["outcome"], "crash");
        assert_eq!(
            finding.evidence["classifier"]["reason"],
            "nonzero_exit_nonterminal"
        );
        assert_eq!(finding.evidence["classifier"]["persisted_verdict"], "dead");
        assert_eq!(
            finding.evidence["classifier"]["persisted_outcome_kind"],
            "crash"
        );
    }
}
