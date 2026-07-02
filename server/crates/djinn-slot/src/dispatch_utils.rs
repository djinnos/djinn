//! Pure and async helper functions for the dispatch orchestrator.
//!
//! Extracted from `djinn-agent`'s `supervisor_runner` so they can be shared
//! across host implementations.  None of these depend on `AgentContext`.

use djinn_core::models::TaskRunStatus;
use djinn_runtime::{
    BiStream, LoopGuardKind, ProviderFailureClass, StreamEvent, SupervisorFlow, TaskRunOutcome,
    TaskRunReport,
};

use tokio_util::sync::CancellationToken;
use tracing::Instrument;

// ─── Pure helper functions ──────────────────────────────────────────────────

/// Stage-aware-resume decision seam: given the flow the coordinator routed to
/// and whether the worker's output is durably present on the mirror task_branch,
/// pick the flow the run actually executes.
pub fn resume_flow(base_flow: SupervisorFlow, worker_output_durable: bool) -> SupervisorFlow {
    if matches!(base_flow, SupervisorFlow::ReviewResponse) && worker_output_durable {
        SupervisorFlow::ReviewResume
    } else {
        base_flow
    }
}

/// Extract the provider failure class from a terminal report, if any.
pub fn provider_failure_class_for_report(report: &TaskRunReport) -> Option<ProviderFailureClass> {
    match &report.outcome {
        TaskRunOutcome::Failed {
            provider_failure: Some(class),
            ..
        } => Some(*class),
        _ => None,
    }
}

/// Whether the report indicates a budget-park outcome.
pub fn is_budget_park_report(report: &TaskRunReport) -> bool {
    matches!(
        &report.outcome,
        TaskRunOutcome::Parked { reason, .. } if reason == "budget"
    )
}

/// Whether the terminal report should feed the model-health success signal.
pub fn terminal_report_feeds_model_success(report: &TaskRunReport) -> bool {
    !report.stages_completed.is_empty()
        && matches!(report_to_terminal_status(report), TaskRunStatus::Completed)
        && !is_budget_park_report(report)
}

/// Map a `TaskRunReport` outcome to the persisted `TaskRunStatus`.
pub fn report_to_terminal_status(report: &TaskRunReport) -> TaskRunStatus {
    match &report.outcome {
        TaskRunOutcome::PrOpened { .. }
        | TaskRunOutcome::Closed { .. }
        | TaskRunOutcome::WorkerSubmitted
        | TaskRunOutcome::Parked { .. }
        | TaskRunOutcome::Escalated { .. } => TaskRunStatus::Completed,
        TaskRunOutcome::Failed { .. } | TaskRunOutcome::LoopGuardTripped { .. } => {
            TaskRunStatus::Failed
        }
        TaskRunOutcome::Interrupted => TaskRunStatus::Interrupted,
    }
}

/// Choose the authoritative terminal report for a completed dispatch.
///
/// The streamed worker report (when present) wins: it carries the canonical
/// run id the sessions were persisted under and the stages actually completed.
/// The runtime's teardown report is a stub fallback used only when the worker
/// died before emitting a report.
pub fn select_terminal_report(
    streamed: Option<TaskRunReport>,
    teardown: TaskRunReport,
) -> TaskRunReport {
    streamed.unwrap_or(teardown)
}

/// Map a `LoopGuardKind` to a human-readable label.
pub fn loop_guard_kind_label(kind: LoopGuardKind) -> &'static str {
    match kind {
        LoopGuardKind::IdenticalToolFailure => "identical_tool_failure",
        LoopGuardKind::PermissionDenial => "permission_denial",
        LoopGuardKind::IdenticalOutput => "identical_output",
        LoopGuardKind::ConsecutiveFailures => "consecutive_failures",
    }
}

/// Build the Planner intervention reason string for a loop-guard trip.
pub fn loop_guard_planner_intervention_reason(
    kind: LoopGuardKind,
    offending_signature: &str,
    threshold: u32,
    observed: u32,
    turn_span: (u32, u32),
    session_id: &str,
) -> String {
    format!(
        "Reply-loop guard `{}` tripped in session `{}`: offending_signature=`{}`, \
         threshold={}, observed={}, turn_span={}..={}. The run completed in a \
         degenerate loop rather than failing to dispatch; do not re-dispatch the \
         identical worker attempt. Decide how to unstick this: DECOMPOSE into \
         focused subtasks, RESCOPE/clarify the acceptance criteria and re-dispatch, \
         or CLOSE if the work is moot/duplicate/already-done.",
        loop_guard_kind_label(kind),
        session_id,
        offending_signature,
        threshold,
        observed,
        turn_span.0,
        turn_span.1,
    )
}

/// Build a tracing span for supervisor RPC operations.
pub fn supervisor_rpc_span(op: &'static str, session_id: &str, task_id: &str) -> tracing::Span {
    tracing::info_span!(
        "djinn.supervisor.rpc",
        op,
        session_id = %session_id,
        task_id = %task_id,
    )
}

// ─── Async helpers ──────────────────────────────────────────────────────────

/// Default pre-session liveness deadline: how long stage init has to reach the
/// first reply-loop session/turn before the run is failed fast.
///
/// Overridable via `DJINN_PRESESSION_DEADLINE_SECS` (0 / unparseable → default).
pub fn pre_session_deadline() -> std::time::Duration {
    const PRE_SESSION_DEADLINE_SECS_DEFAULT: u64 = 480;
    let secs = std::env::var("DJINN_PRESESSION_DEADLINE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .filter(|&s| s > 0)
        .unwrap_or(PRE_SESSION_DEADLINE_SECS_DEFAULT);
    std::time::Duration::from_secs(secs)
}

/// Initial step label used before the worker emits its first stage marker.
pub const PRE_SESSION_INITIAL_STEP: &str = "run_create";

/// Typed error surfaced when stage init never reaches the first reply-loop turn
/// within the pre-session liveness deadline.
#[derive(Debug, Clone, thiserror::Error)]
#[error(
    "pre-session stage-init deadline exceeded: no session / first provider turn after \
     {elapsed_secs}s (hung at step '{step}')"
)]
pub struct PreSessionTimeout {
    pub step: String,
    pub elapsed_secs: u64,
}

/// Outcome of awaiting the worker's report stream.
#[derive(Debug)]
pub enum ReportAwait {
    Report(Option<TaskRunReport>),
    PreSessionTimeout(PreSessionTimeout),
}

/// Drain a [`BiStream`] until we see the terminal [`StreamEvent::Report`]
/// frame, returning the [`TaskRunReport`] it carries.  Enforces a pre-session
/// liveness deadline: if no session row exists and no first-turn marker has been
/// observed by the deadline, returns [`ReportAwait::PreSessionTimeout`] so the
/// caller can fail fast and redispatch.
///
/// Returns:
/// - `Ok(Some(report))` — the worker emitted its terminal report.
/// - `Ok(None)` — the channel closed or the `kill` token fired before any report.
pub async fn await_report_from_stream<F>(
    mut stream: BiStream,
    kill: &CancellationToken,
    task_run_id: &str,
    task_id: &str,
    deadline: std::time::Duration,
    mut exists_session: F,
) -> anyhow::Result<ReportAwait>
where
    F: FnMut() -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>>,
{
    let started = tokio::time::Instant::now();
    let sleep = tokio::time::sleep(deadline);
    tokio::pin!(sleep);

    let mut last_step = PRE_SESSION_INITIAL_STEP.to_string();
    let mut session_reached = false;

    loop {
        tokio::select! {
            biased;
            _ = kill.cancelled() => {
                let rpc_span = supervisor_rpc_span("kill", task_run_id, task_id);
                async move {
                    tracing::debug!(
                        op = "kill",
                        session_id = %task_run_id,
                        task_id = %task_id,
                        "supervisor dispatch: kill fired while awaiting terminal report; \
                         proceeding to teardown"
                    );
                }
                .instrument(rpc_span)
                .await;
                return Ok(ReportAwait::Report(None));
            }
            _ = &mut sleep, if !session_reached => {
                if exists_session().await {
                    session_reached = true;
                    continue;
                }
                let elapsed_secs = started.elapsed().as_secs();
                tracing::warn!(
                    task_id = %task_id,
                    task_run_id = %task_run_id,
                    stage_step = %last_step,
                    elapsed_secs,
                    "supervisor dispatch: pre-session liveness deadline breached before first turn"
                );
                return Ok(ReportAwait::PreSessionTimeout(PreSessionTimeout {
                    step: last_step.clone(),
                    elapsed_secs,
                }));
            }
            frame = stream.events_rx.recv() => {
                match frame {
                    Some(StreamEvent::Report(report)) => {
                        let rpc_span = supervisor_rpc_span("terminal_report", task_run_id, task_id);
                        let report_task_run_id = report.task_run_id.clone();
                        async move {
                            tracing::info!(
                                event = "supervisor.rpc.terminal_report",
                                op = "terminal_report",
                                session_id = %task_run_id,
                                task_id = %task_id,
                                task_run_id = %report_task_run_id,
                            );
                        }
                        .instrument(rpc_span)
                        .await;
                        return Ok(ReportAwait::Report(Some(report)));
                    }
                    Some(StreamEvent::StageStep { step }) => {
                        if step == djinn_runtime::STAGE_STEP_FIRST_TURN {
                            session_reached = true;
                        }
                        last_step = step;
                    }
                    Some(
                        StreamEvent::AssistantDelta { .. }
                        | StreamEvent::ToolCall { .. }
                        | StreamEvent::FinalizePayload { .. }
                        | StreamEvent::StageOutcome { .. },
                    ) => {
                        session_reached = true;
                    }
                    None => return Ok(ReportAwait::Report(None)),
                }
            }
        }
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use djinn_runtime::RoleKind;

    fn report(id: &str, stages: Vec<RoleKind>, outcome: TaskRunOutcome) -> TaskRunReport {
        TaskRunReport {
            task_run_id: id.to_string(),
            outcome,
            stages_completed: stages,
        }
    }

    fn never_exists() -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>> {
        Box::pin(async { false })
    }

    fn always_exists() -> std::pin::Pin<Box<dyn std::future::Future<Output = bool> + Send>> {
        Box::pin(async { true })
    }

    #[test]
    fn planned_terminal_outcomes_have_no_provider_breaker_signal() {
        let guard_report = report(
            "guard-run",
            vec![RoleKind::Worker],
            TaskRunOutcome::LoopGuardTripped {
                kind: LoopGuardKind::IdenticalToolFailure,
                offending_signature: "shell:cargo-test".into(),
                threshold: 3,
                observed: 4,
                turn_span: (7, 12),
                session_id: "session-123".into(),
            },
        );
        assert_eq!(
            provider_failure_class_for_report(&guard_report),
            None,
            "loop-guard trips must not feed the provider breaker"
        );

        let ordinary_completed = report(
            "closed-run",
            vec![RoleKind::Worker],
            TaskRunOutcome::Closed {
                reason: "done".into(),
            },
        );
        assert!(
            terminal_report_feeds_model_success(&ordinary_completed),
            "ordinary completed runs still record model-health success"
        );

        for (outcome, label) in [
            (
                TaskRunOutcome::Parked {
                    reason: "budget".into(),
                    wind_down_ignored: false,
                    session_id: "session-budget-summary".into(),
                    tokens_in: 4_096,
                    tokens_out: 512,
                },
                "successful-summary budget parks",
            ),
            (
                TaskRunOutcome::Parked {
                    reason: "budget".into(),
                    wind_down_ignored: true,
                    session_id: "session-budget-ignored".into(),
                    tokens_in: 4_096,
                    tokens_out: 12,
                },
                "ignored-wind-down budget parks",
            ),
        ] {
            let budget_report = report("budget-run", vec![RoleKind::Worker], outcome);
            assert_eq!(
                report_to_terminal_status(&budget_report),
                TaskRunStatus::Completed,
                "{label} are planned completed lifecycle endings"
            );
            assert_eq!(
                provider_failure_class_for_report(&budget_report),
                None,
                "{label} must not feed provider breaker failure accounting"
            );
            assert!(
                !terminal_report_feeds_model_success(&budget_report),
                "{label} must not feed model-health success accounting either"
            );
        }

        let failed_report = report(
            "failed-run",
            vec![RoleKind::Worker],
            TaskRunOutcome::Failed {
                stage: "worker".into(),
                reason: "provider rejected request".into(),
                provider_failure: Some(ProviderFailureClass::Failure),
                error_class: None,
                hint: None,
                body_excerpt: None,
            },
        );
        assert_eq!(
            provider_failure_class_for_report(&failed_report),
            Some(ProviderFailureClass::Failure),
            "typed provider failures still feed the breaker"
        );
    }

    #[test]
    fn loop_guard_reason_names_full_trip_payload() {
        let reason = loop_guard_planner_intervention_reason(
            LoopGuardKind::IdenticalToolFailure,
            "shell:cargo-test",
            3,
            4,
            (7, 12),
            "session-123",
        );

        for expected in [
            "identical_tool_failure",
            "shell:cargo-test",
            "threshold=3",
            "observed=4",
            "turn_span=7..=12",
            "session-123",
            "do not re-dispatch the identical worker attempt",
        ] {
            assert!(
                reason.contains(expected),
                "reason must include `{expected}`; got {reason}"
            );
        }
    }

    #[test]
    fn streamed_report_wins_over_teardown_stub() {
        let streamed = report(
            "id-B-persisted",
            vec![RoleKind::Worker, RoleKind::Reviewer],
            TaskRunOutcome::PrOpened {
                url: "https://example/pr/1".into(),
                sha: "deadbeef".into(),
            },
        );
        let teardown_stub = report("id-A-transport", vec![], TaskRunOutcome::Interrupted);

        let chosen = select_terminal_report(Some(streamed), teardown_stub);

        assert_eq!(
            chosen.task_run_id, "id-B-persisted",
            "extraction id must come from the streamed report, not the teardown stub"
        );
        assert!(
            !chosen.stages_completed.is_empty(),
            "real stages must survive so the extraction gate opens"
        );
    }

    #[test]
    fn teardown_stub_used_when_no_streamed_report() {
        let teardown_stub = report("id-A-transport", vec![], TaskRunOutcome::Interrupted);
        let chosen = select_terminal_report(None, teardown_stub);
        assert_eq!(chosen.task_run_id, "id-A-transport");
        assert!(chosen.stages_completed.is_empty());
    }

    // ── Stage-aware resume decision ───────────────────────────────────────────

    #[test]
    fn resume_flow_upgrades_review_response_to_reviewer_only_when_durable() {
        assert_eq!(
            resume_flow(SupervisorFlow::ReviewResponse, true),
            SupervisorFlow::ReviewResume
        );
        assert_eq!(
            SupervisorFlow::ReviewResume.role_sequence(),
            &[RoleKind::Reviewer],
            "ReviewResume must skip the worker stage"
        );
    }

    #[test]
    fn resume_flow_keeps_review_response_when_output_not_durable() {
        assert_eq!(
            resume_flow(SupervisorFlow::ReviewResponse, false),
            SupervisorFlow::ReviewResponse
        );
    }

    #[test]
    fn resume_flow_leaves_non_review_response_flows_untouched() {
        for flow in [
            SupervisorFlow::NewTask,
            SupervisorFlow::ConflictRetry,
            SupervisorFlow::Spike,
            SupervisorFlow::Planning,
            SupervisorFlow::Lead,
        ] {
            assert_eq!(resume_flow(flow, true), flow);
            assert_eq!(resume_flow(flow, false), flow);
        }
    }

    #[tokio::test]
    async fn await_report_returns_streamed_report() {
        let (bistream, events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        let streamed = report(
            "id-B",
            vec![RoleKind::Worker],
            TaskRunOutcome::Closed {
                reason: "done".into(),
            },
        );
        events_tx
            .send(StreamEvent::Report(streamed))
            .await
            .expect("send report");

        let got = await_report_from_stream(
            bistream,
            &kill,
            "session-id-b",
            "task-id-b",
            std::time::Duration::from_secs(60),
            never_exists,
        )
        .await
        .expect("await ok");
        let report = match got {
            ReportAwait::Report(Some(r)) => r,
            other => panic!("expected report, got {other:?}"),
        };
        assert_eq!(report.task_run_id, "id-B");
    }

    #[tokio::test]
    async fn await_report_drops_non_terminal_frames_then_returns_report() {
        let (bistream, events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        events_tx
            .send(StreamEvent::AssistantDelta {
                session_id: "s1".into(),
                text: "thinking".into(),
            })
            .await
            .unwrap();
        events_tx
            .send(StreamEvent::Report(report(
                "id-B",
                vec![RoleKind::Worker],
                TaskRunOutcome::Interrupted,
            )))
            .await
            .unwrap();

        let got = await_report_from_stream(
            bistream,
            &kill,
            "session-id-b",
            "task-id-b",
            std::time::Duration::from_secs(60),
            never_exists,
        )
        .await
        .expect("await ok");
        let report = match got {
            ReportAwait::Report(Some(r)) => r,
            other => panic!("expected report, got {other:?}"),
        };
        assert_eq!(report.task_run_id, "id-B");
    }

    #[tokio::test]
    async fn await_report_returns_none_when_kill_fires() {
        let (bistream, _events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        kill.cancel();
        let got = await_report_from_stream(
            bistream,
            &kill,
            "session-kill",
            "task-kill",
            std::time::Duration::from_secs(60),
            never_exists,
        )
        .await
        .expect("await ok");
        match got {
            ReportAwait::Report(None) => {}
            other => panic!("expected Report(None), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn await_report_returns_none_when_channel_closes_without_report() {
        let (bistream, events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        drop(events_tx);
        let got = await_report_from_stream(
            bistream,
            &kill,
            "session-closed",
            "task-closed",
            std::time::Duration::from_secs(60),
            never_exists,
        )
        .await
        .expect("await ok");
        match got {
            ReportAwait::Report(None) => {}
            other => panic!("expected Report(None), got {other:?}"),
        }
    }

    #[tokio::test]
    async fn await_report_breaches_presession_deadline_when_no_session() {
        let (bistream, _events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        let got = await_report_from_stream(
            bistream,
            &kill,
            "session-presession",
            "task-presession",
            std::time::Duration::from_millis(10),
            never_exists,
        )
        .await
        .expect("await ok");
        match got {
            ReportAwait::PreSessionTimeout(t) => {
                assert_eq!(t.step, PRE_SESSION_INITIAL_STEP);
            }
            other => panic!("expected pre-session timeout, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn await_report_disarms_presession_deadline_once_session_exists() {
        let (bistream, events_tx, _requests_rx) = BiStream::new_in_memory(8);
        let kill = CancellationToken::new();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            let _ = events_tx
                .send(StreamEvent::StageStep {
                    step: "reply_loop".into(),
                })
                .await;
            let _ = events_tx
                .send(StreamEvent::Report(report(
                    "id-B",
                    vec![RoleKind::Worker],
                    TaskRunOutcome::Closed {
                        reason: "done".into(),
                    },
                )))
                .await;
        });

        let got = await_report_from_stream(
            bistream,
            &kill,
            "session-presession",
            "task-presession",
            std::time::Duration::from_millis(20),
            always_exists,
        )
        .await
        .expect("await ok");
        match got {
            ReportAwait::Report(Some(r)) => assert_eq!(r.task_run_id, "id-B"),
            other => panic!("expected report, got {other:?}"),
        }
    }
}
