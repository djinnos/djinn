//! Shared, pure liveness contract for proposal-refinement runs.
//!
//! This module deliberately contains no repository or coordinator behavior.
//! Callers assemble a [`RefinementLivenessSnapshot`] from durable evidence and
//! inject one database-observation timestamp into
//! [`evaluate_refinement_liveness`].

use serde::{Deserialize, Serialize};

/// A database timestamp represented as Unix milliseconds.
///
/// Keeping the wire form numeric makes snapshots serde-stable without coupling
/// this domain contract to a particular SQL timestamp representation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct DbTimestamp(pub i64);

/// Durable state of a refinement run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefinementRunState {
    Active,
    Parked,
    Terminal,
}

/// Durable state of a dispatch intent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefinementIntentState {
    Pending,
    Claimed,
    Completed,
    Cancelled,
    Failed,
}

/// A deliberate non-dispatching refinement state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefinementParkKind {
    AwaitingReview,
    AwaitingEvidence,
}

/// Tribunal phase attached to a dispatch intent or between-phase handoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefinementPhase {
    AdversaryAttack,
    AdvocateRevision,
    JudgeAdjudication,
}

/// Tribunal role attached to a dispatch intent or between-phase handoff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefinementRole {
    Adversary,
    Advocate,
    Judge,
}

/// Canonical structured reason that a refinement run stopped.
///
/// The externally persisted tag is the snake-case variant name. Unknown tags
/// intentionally fail serde deserialization; `UnknownLegacy` is exclusively a
/// migration-time representation for already-known legacy values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "tag", content = "context", rename_all = "snake_case")]
pub enum RefinementStopReason {
    AdversaryDry,
    RoundCap,
    SpawnCap,
    RepeatedObjection {
        signature: String,
        occurrences: u64,
    },
    AgentFailure {
        role: RefinementRole,
        error_code: String,
        message: String,
    },
    HumanAccepted,
    HumanRejected,
    Interrupted {
        detail: Option<String>,
    },
    ReapedPhantom {
        prior_run_id: String,
        generation: u64,
        evidence_summary: String,
    },
    OperatorStop {
        actor: String,
        reason: Option<String>,
    },
    UnknownLegacy {
        original_value: String,
        source_row: String,
    },
}

impl RefinementStopReason {
    /// Canonical machine-readable tag used by durable storage.
    pub const fn tag(&self) -> &'static str {
        match self {
            Self::AdversaryDry => "adversary_dry",
            Self::RoundCap => "round_cap",
            Self::SpawnCap => "spawn_cap",
            Self::RepeatedObjection { .. } => "repeated_objection",
            Self::AgentFailure { .. } => "agent_failure",
            Self::HumanAccepted => "human_accepted",
            Self::HumanRejected => "human_rejected",
            Self::Interrupted { .. } => "interrupted",
            Self::ReapedPhantom { .. } => "reaped_phantom",
            Self::OperatorStop { .. } => "operator_stop",
            Self::UnknownLegacy { .. } => "unknown_legacy",
        }
    }
}

/// Run evidence loaded from `refinement_runs`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefinementRunSnapshot {
    pub run_id: String,
    pub state: RefinementRunState,
    pub terminal_reason: Option<RefinementStopReason>,
}

/// Dispatch-intent evidence loaded from `refinement_dispatch_intents`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefinementIntentSnapshot {
    pub intent_id: String,
    pub run_id: String,
    pub state: RefinementIntentState,
    pub phase: RefinementPhase,
    pub role: RefinementRole,
    /// Required for claimed intents; an absent lease is expired/not live.
    pub lease_expires_at: Option<DbTimestamp>,
}

/// Explicit park evidence. A park is live even if no task or session exists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefinementParkSnapshot {
    pub run_id: String,
    pub kind: RefinementParkKind,
}

/// Task states that prove a refinement dispatch remains in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefinementTaskState {
    Open,
    Queued,
    Running,
    PoolPaused,
    Closed,
    Cancelled,
}

/// Task evidence. `run_id` must exactly equal the snapshot run id to count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefinementTaskEvidence {
    pub task_id: String,
    pub run_id: String,
    pub intent_id: Option<String>,
    pub state: RefinementTaskState,
}

/// Session states relevant to refinement liveness.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefinementSessionState {
    Live,
    Ended,
}

/// Session evidence. It counts only when it names this run and joins a task
/// that also names this run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefinementSessionEvidence {
    pub session_id: String,
    pub task_id: String,
    pub run_id: String,
    pub state: RefinementSessionState,
}

/// A durable phase handoff that has already selected its next dispatch intent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefinementBetweenPhaseSnapshot {
    pub run_id: String,
    pub next_intent: RefinementIntentSnapshot,
}

/// DB-time heartbeat grace. It is intentionally separate from process-local
/// clocks so every observer makes the same decision for a snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefinementHeartbeatSnapshot {
    pub run_id: String,
    pub heartbeat_at: DbTimestamp,
    pub grace_millis: i64,
}

/// All evidence needed for a deterministic liveness evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefinementLivenessSnapshot {
    pub run: RefinementRunSnapshot,
    pub park: Option<RefinementParkSnapshot>,
    #[serde(default)]
    pub intents: Vec<RefinementIntentSnapshot>,
    #[serde(default)]
    pub tasks: Vec<RefinementTaskEvidence>,
    #[serde(default)]
    pub sessions: Vec<RefinementSessionEvidence>,
    pub between_phase: Option<RefinementBetweenPhaseSnapshot>,
    pub heartbeat: Option<RefinementHeartbeatSnapshot>,
}

/// Evidence that caused a run to be considered live, in precedence order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefinementLivenessEvidence {
    AwaitingReviewPark,
    AwaitingEvidencePark,
    PendingIntent { intent_id: String },
    ClaimedIntent { intent_id: String },
    OpenTask { task_id: String },
    QueuedTask { task_id: String },
    RunningTask { task_id: String },
    PoolPausedTask { task_id: String },
    LiveSession { session_id: String, task_id: String },
    BetweenPhase { intent_id: String },
    FreshHeartbeat { heartbeat_at: DbTimestamp },
}

/// Why a nonterminal run has no live evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefinementStaleReason {
    NoLiveEvidence,
}

/// The complete deterministic liveness outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefinementLivenessResult {
    Terminal {
        reason: Option<RefinementStopReason>,
    },
    Live {
        evidence: RefinementLivenessEvidence,
    },
    Stale {
        reason: RefinementStaleReason,
    },
}

/// Evaluate a refinement run from a durable snapshot and injected DB time.
///
/// The ordering is deliberate: terminal state wins globally; then explicit
/// parks, intents, active tasks, exact-run sessions, between-phase handoffs,
/// and heartbeat grace. No wall clock is read here.
pub fn evaluate_refinement_liveness(
    snapshot: &RefinementLivenessSnapshot,
    now: DbTimestamp,
) -> RefinementLivenessResult {
    let run_id = &snapshot.run.run_id;

    if snapshot.run.state == RefinementRunState::Terminal {
        return RefinementLivenessResult::Terminal {
            reason: snapshot.run.terminal_reason.clone(),
        };
    }

    if let Some(park) = snapshot.park.as_ref().filter(|park| &park.run_id == run_id) {
        let evidence = match park.kind {
            RefinementParkKind::AwaitingReview => RefinementLivenessEvidence::AwaitingReviewPark,
            RefinementParkKind::AwaitingEvidence => {
                RefinementLivenessEvidence::AwaitingEvidencePark
            }
        };
        return RefinementLivenessResult::Live { evidence };
    }

    if let Some(intent) = snapshot
        .intents
        .iter()
        .find(|intent| &intent.run_id == run_id && intent.state == RefinementIntentState::Pending)
    {
        return RefinementLivenessResult::Live {
            evidence: RefinementLivenessEvidence::PendingIntent {
                intent_id: intent.intent_id.clone(),
            },
        };
    }

    if let Some(intent) = snapshot.intents.iter().find(|intent| {
        &intent.run_id == run_id
            && intent.state == RefinementIntentState::Claimed
            && intent.lease_expires_at.is_some_and(|expiry| expiry > now)
    }) {
        return RefinementLivenessResult::Live {
            evidence: RefinementLivenessEvidence::ClaimedIntent {
                intent_id: intent.intent_id.clone(),
            },
        };
    }

    if let Some(task) = snapshot.tasks.iter().find(|task| {
        &task.run_id == run_id
            && matches!(
                task.state,
                RefinementTaskState::Open
                    | RefinementTaskState::Queued
                    | RefinementTaskState::Running
                    | RefinementTaskState::PoolPaused
            )
    }) {
        let evidence = match task.state {
            RefinementTaskState::Open => RefinementLivenessEvidence::OpenTask {
                task_id: task.task_id.clone(),
            },
            RefinementTaskState::Queued => RefinementLivenessEvidence::QueuedTask {
                task_id: task.task_id.clone(),
            },
            RefinementTaskState::Running => RefinementLivenessEvidence::RunningTask {
                task_id: task.task_id.clone(),
            },
            RefinementTaskState::PoolPaused => RefinementLivenessEvidence::PoolPausedTask {
                task_id: task.task_id.clone(),
            },
            RefinementTaskState::Closed | RefinementTaskState::Cancelled => unreachable!(),
        };
        return RefinementLivenessResult::Live { evidence };
    }

    if let Some(session) = snapshot.sessions.iter().find(|session| {
        session.state == RefinementSessionState::Live
            && &session.run_id == run_id
            && snapshot
                .tasks
                .iter()
                .any(|task| task.task_id == session.task_id && &task.run_id == run_id)
    }) {
        return RefinementLivenessResult::Live {
            evidence: RefinementLivenessEvidence::LiveSession {
                session_id: session.session_id.clone(),
                task_id: session.task_id.clone(),
            },
        };
    }

    if let Some(handoff) = snapshot.between_phase.as_ref().filter(|handoff| {
        &handoff.run_id == run_id
            && handoff.next_intent.run_id == *run_id
            && intent_is_live(&handoff.next_intent, now)
    }) {
        return RefinementLivenessResult::Live {
            evidence: RefinementLivenessEvidence::BetweenPhase {
                intent_id: handoff.next_intent.intent_id.clone(),
            },
        };
    }

    if let Some(heartbeat) = snapshot.heartbeat.as_ref().filter(|heartbeat| {
        &heartbeat.run_id == run_id
            && heartbeat.grace_millis >= 0
            && heartbeat
                .heartbeat_at
                .0
                .saturating_add(heartbeat.grace_millis)
                >= now.0
    }) {
        return RefinementLivenessResult::Live {
            evidence: RefinementLivenessEvidence::FreshHeartbeat {
                heartbeat_at: heartbeat.heartbeat_at,
            },
        };
    }

    RefinementLivenessResult::Stale {
        reason: RefinementStaleReason::NoLiveEvidence,
    }
}

fn intent_is_live(intent: &RefinementIntentSnapshot, now: DbTimestamp) -> bool {
    intent.state == RefinementIntentState::Pending
        || (intent.state == RefinementIntentState::Claimed
            && intent.lease_expires_at.is_some_and(|expiry| expiry > now))
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: DbTimestamp = DbTimestamp(10_000);

    fn snapshot() -> RefinementLivenessSnapshot {
        RefinementLivenessSnapshot {
            run: RefinementRunSnapshot {
                run_id: "run-current".into(),
                state: RefinementRunState::Active,
                terminal_reason: None,
            },
            park: None,
            intents: vec![],
            tasks: vec![],
            sessions: vec![],
            between_phase: None,
            heartbeat: None,
        }
    }

    fn intent(state: RefinementIntentState) -> RefinementIntentSnapshot {
        RefinementIntentSnapshot {
            intent_id: "intent-1".into(),
            run_id: "run-current".into(),
            state,
            phase: RefinementPhase::AdversaryAttack,
            role: RefinementRole::Adversary,
            lease_expires_at: None,
        }
    }

    fn task(state: RefinementTaskState) -> RefinementTaskEvidence {
        RefinementTaskEvidence {
            task_id: "task-1".into(),
            run_id: "run-current".into(),
            intent_id: Some("intent-1".into()),
            state,
        }
    }

    #[test]
    fn exact_run_live_session_counts_but_prior_run_session_is_ignored() {
        let mut current = snapshot();
        current.tasks.push(task(RefinementTaskState::Closed));
        current.sessions.push(RefinementSessionEvidence {
            session_id: "session-current".into(),
            task_id: "task-1".into(),
            run_id: "run-current".into(),
            state: RefinementSessionState::Live,
        });
        assert!(matches!(
            evaluate_refinement_liveness(&current, NOW),
            RefinementLivenessResult::Live {
                evidence: RefinementLivenessEvidence::LiveSession { .. }
            }
        ));

        current.sessions[0].run_id = "run-prior".into();
        assert!(matches!(
            evaluate_refinement_liveness(&current, NOW),
            RefinementLivenessResult::Stale { .. }
        ));
    }

    #[test]
    fn parks_take_precedence_over_other_evidence() {
        for kind in [
            RefinementParkKind::AwaitingReview,
            RefinementParkKind::AwaitingEvidence,
        ] {
            let mut value = snapshot();
            value.park = Some(RefinementParkSnapshot {
                run_id: "run-current".into(),
                kind,
            });
            value.intents.push(intent(RefinementIntentState::Pending));
            let expected = match kind {
                RefinementParkKind::AwaitingReview => {
                    RefinementLivenessEvidence::AwaitingReviewPark
                }
                RefinementParkKind::AwaitingEvidence => {
                    RefinementLivenessEvidence::AwaitingEvidencePark
                }
            };
            assert_eq!(
                evaluate_refinement_liveness(&value, NOW),
                RefinementLivenessResult::Live { evidence: expected }
            );
        }
    }

    #[test]
    fn pending_and_unexpired_claimed_intents_are_live_but_old_pending_is_not() {
        let mut value = snapshot();
        let mut old = intent(RefinementIntentState::Pending);
        old.run_id = "run-prior".into();
        value.intents.push(old);
        assert!(matches!(
            evaluate_refinement_liveness(&value, NOW),
            RefinementLivenessResult::Stale { .. }
        ));

        value.intents.push(intent(RefinementIntentState::Pending));
        assert!(matches!(
            evaluate_refinement_liveness(&value, NOW),
            RefinementLivenessResult::Live {
                evidence: RefinementLivenessEvidence::PendingIntent { .. }
            }
        ));

        value.intents.clear();
        let mut claimed = intent(RefinementIntentState::Claimed);
        claimed.lease_expires_at = Some(DbTimestamp(10_001));
        value.intents.push(claimed);
        assert!(matches!(
            evaluate_refinement_liveness(&value, NOW),
            RefinementLivenessResult::Live {
                evidence: RefinementLivenessEvidence::ClaimedIntent { .. }
            }
        ));
    }

    #[test]
    fn expired_claim_alone_is_stale() {
        let mut value = snapshot();
        let mut claimed = intent(RefinementIntentState::Claimed);
        claimed.lease_expires_at = Some(NOW);
        value.intents.push(claimed);
        assert_eq!(
            evaluate_refinement_liveness(&value, NOW),
            RefinementLivenessResult::Stale {
                reason: RefinementStaleReason::NoLiveEvidence
            }
        );
    }

    #[test]
    fn open_queued_running_and_pool_paused_tasks_are_live_without_sessions() {
        for state in [
            RefinementTaskState::Open,
            RefinementTaskState::Queued,
            RefinementTaskState::Running,
            RefinementTaskState::PoolPaused,
        ] {
            let mut value = snapshot();
            value.tasks.push(task(state));
            assert!(matches!(
                evaluate_refinement_liveness(&value, NOW),
                RefinementLivenessResult::Live { .. }
            ));
        }
    }

    #[test]
    fn valid_between_phase_and_fresh_heartbeat_are_live() {
        let mut value = snapshot();
        value.between_phase = Some(RefinementBetweenPhaseSnapshot {
            run_id: "run-current".into(),
            next_intent: intent(RefinementIntentState::Pending),
        });
        assert!(matches!(
            evaluate_refinement_liveness(&value, NOW),
            RefinementLivenessResult::Live {
                evidence: RefinementLivenessEvidence::BetweenPhase { .. }
            }
        ));

        value.between_phase = None;
        value.heartbeat = Some(RefinementHeartbeatSnapshot {
            run_id: "run-current".into(),
            heartbeat_at: DbTimestamp(9_500),
            grace_millis: 500,
        });
        assert!(matches!(
            evaluate_refinement_liveness(&value, NOW),
            RefinementLivenessResult::Live {
                evidence: RefinementLivenessEvidence::FreshHeartbeat { .. }
            }
        ));
    }

    #[test]
    fn liveness_evidence_uses_the_documented_precedence_order() {
        let mut value = snapshot();

        let mut pending = intent(RefinementIntentState::Pending);
        pending.intent_id = "intent-priority".into();
        value.intents.push(pending);

        let mut active_task = task(RefinementTaskState::Open);
        active_task.task_id = "task-priority".into();
        value.tasks.push(active_task);

        let mut session_task = task(RefinementTaskState::Closed);
        session_task.task_id = "task-session".into();
        value.tasks.push(session_task);
        value.sessions.push(RefinementSessionEvidence {
            session_id: "session-priority".into(),
            task_id: "task-session".into(),
            run_id: "run-current".into(),
            state: RefinementSessionState::Live,
        });

        let mut handoff_intent = intent(RefinementIntentState::Pending);
        handoff_intent.intent_id = "intent-handoff".into();
        value.between_phase = Some(RefinementBetweenPhaseSnapshot {
            run_id: "run-current".into(),
            next_intent: handoff_intent,
        });
        value.heartbeat = Some(RefinementHeartbeatSnapshot {
            run_id: "run-current".into(),
            heartbeat_at: DbTimestamp(9_500),
            grace_millis: 500,
        });

        assert_eq!(
            evaluate_refinement_liveness(&value, NOW),
            RefinementLivenessResult::Live {
                evidence: RefinementLivenessEvidence::PendingIntent {
                    intent_id: "intent-priority".into(),
                },
            }
        );

        value.intents.clear();
        assert_eq!(
            evaluate_refinement_liveness(&value, NOW),
            RefinementLivenessResult::Live {
                evidence: RefinementLivenessEvidence::OpenTask {
                    task_id: "task-priority".into(),
                },
            }
        );

        value.tasks.remove(0);
        assert_eq!(
            evaluate_refinement_liveness(&value, NOW),
            RefinementLivenessResult::Live {
                evidence: RefinementLivenessEvidence::LiveSession {
                    session_id: "session-priority".into(),
                    task_id: "task-session".into(),
                },
            }
        );

        value.sessions.clear();
        assert_eq!(
            evaluate_refinement_liveness(&value, NOW),
            RefinementLivenessResult::Live {
                evidence: RefinementLivenessEvidence::BetweenPhase {
                    intent_id: "intent-handoff".into(),
                },
            }
        );

        value.between_phase = None;
        assert_eq!(
            evaluate_refinement_liveness(&value, NOW),
            RefinementLivenessResult::Live {
                evidence: RefinementLivenessEvidence::FreshHeartbeat {
                    heartbeat_at: DbTimestamp(9_500),
                },
            }
        );
    }

    #[test]
    fn terminal_always_wins_and_absent_evidence_is_stale() {
        let mut value = snapshot();
        value.run.state = RefinementRunState::Terminal;
        value.run.terminal_reason = Some(RefinementStopReason::HumanAccepted);
        value.intents.push(intent(RefinementIntentState::Pending));
        assert!(matches!(
            evaluate_refinement_liveness(&value, NOW),
            RefinementLivenessResult::Terminal { .. }
        ));

        value.run.state = RefinementRunState::Active;
        value.run.terminal_reason = None;
        value.intents.clear();
        assert!(matches!(
            evaluate_refinement_liveness(&value, NOW),
            RefinementLivenessResult::Stale { .. }
        ));
    }

    #[test]
    fn all_canonical_stop_reasons_round_trip_and_unknown_tag_fails_closed() {
        let all = vec![
            RefinementStopReason::AdversaryDry,
            RefinementStopReason::RoundCap,
            RefinementStopReason::SpawnCap,
            RefinementStopReason::RepeatedObjection {
                signature: "sig".into(),
                occurrences: 2,
            },
            RefinementStopReason::AgentFailure {
                role: RefinementRole::Judge,
                error_code: "timeout".into(),
                message: "timed out".into(),
            },
            RefinementStopReason::HumanAccepted,
            RefinementStopReason::HumanRejected,
            RefinementStopReason::Interrupted {
                detail: Some("restart".into()),
            },
            RefinementStopReason::ReapedPhantom {
                prior_run_id: "run-prior".into(),
                generation: 3,
                evidence_summary: "no session".into(),
            },
            RefinementStopReason::OperatorStop {
                actor: "operator".into(),
                reason: None,
            },
            RefinementStopReason::UnknownLegacy {
                original_value: "old_tag".into(),
                source_row: "row-1".into(),
            },
        ];
        for reason in all {
            let json = serde_json::to_string(&reason).unwrap();
            assert_eq!(
                serde_json::from_str::<RefinementStopReason>(&json).unwrap(),
                reason
            );
        }
        assert!(
            serde_json::from_str::<RefinementStopReason>(
                r#"{"tag":"new_future_stop","context":{}}"#
            )
            .is_err()
        );
    }
}
