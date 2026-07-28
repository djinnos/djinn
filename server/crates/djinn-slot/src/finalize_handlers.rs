use crate::finalize_types::{AcVerdict, SubmitDecision, SubmitGrooming, SubmitReview, SubmitWork};
use crate::host::SlotContext;
use djinn_db::TaskRepository;
use djinn_db::repositories::task_rejected_submission_integrity::{
    RecordTaskRejectedSubmissionParams, TaskRejectedSubmissionIntegrityRepository,
};
use djinn_db::repositories::task_run::TaskRunRepository;

/// Process the structured finalize tool payload captured by the reply loop (ADR-036).
///
/// Called from the task lifecycle after the reply loop exits cleanly. Logs structured
/// activity entries and performs side effects specific to each finalize tool:
/// - `submit_work`: logs work summary and files changed
/// - `submit_review`: atomically sets AC met/unmet state, logs verdict
/// - `submit_decision`: logs lead decision and rationale
/// - `submit_grooming`: logs per-task grooming entries
///
/// Silently no-ops if `payload` is `None` (session ended without a finalize tool call).
/// Malformed payloads are logged as warnings and do not crash the lifecycle.
pub async fn process_finalize_payload(
    payload: &Option<serde_json::Value>,
    finalize_tool_name: &str,
    task_id: &str,
    authenticated_session_id: &str,
    app_state: &SlotContext,
) {
    let _ = process_finalize_payload_with_outcome(
        payload,
        finalize_tool_name,
        task_id,
        authenticated_session_id,
        app_state,
    )
    .await;
}

pub async fn process_finalize_payload_with_outcome(
    payload: &Option<serde_json::Value>,
    finalize_tool_name: &str,
    task_id: &str,
    authenticated_session_id: &str,
    app_state: &SlotContext,
) -> bool {
    let Some(payload) = payload else { return true };
    match finalize_tool_name {
        "submit_work" => {
            handle_submit_work(payload, task_id, authenticated_session_id, app_state).await
        }
        "submit_review" => {
            handle_submit_review(payload, task_id, app_state).await;
            true
        }
        "submit_decision" => {
            handle_submit_decision(payload, task_id, app_state).await;
            true
        }
        "submit_grooming" => {
            handle_submit_grooming(payload, task_id, app_state).await;
            true
        }
        other => {
            tracing::debug!(
                finalize_tool = %other,
                "finalize_handlers: unrecognized finalize tool; skipping"
            );
            true
        }
    }
}

/// Persist a budget-park handoff without representing it as a successful
/// submission. A park has no completion intent/evidence, so it must never log
/// `work_submitted` or advance an attempt to `submitted`; those side effects
/// are reserved for the C2-validated `handle_submit_work` boundary.
pub async fn handle_budget_park(
    summary: &str,
    details: &str,
    task_id: &str,
    app_state: &SlotContext,
) {
    let summary = summary.trim();
    if summary.is_empty() {
        return;
    }
    let activity_payload = serde_json::json!({
        "summary": summary,
        "remaining_concerns": format!("budget-parked: {details}"),
    })
    .to_string();
    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if let Err(e) = repo
        .log_activity(
            Some(task_id),
            "agent-supervisor",
            "worker",
            "work_parked",
            &activity_payload,
        )
        .await
    {
        tracing::warn!(
            task_id = %task_id,
            error = %e,
            "finalize_handlers: failed to log budget-park handoff activity"
        );
    }
}

/// Log structured work-submission activity for a worker session.
pub(crate) async fn handle_submit_work(
    payload: &serde_json::Value,
    task_id: &str,
    authenticated_session_id: &str,
    app_state: &SlotContext,
) -> bool {
    // This boundary deliberately receives identity separately from the
    // agent-controlled JSON payload. A subsequent finalization slice consumes
    // the server-owned value; this slice only establishes the boundary.
    let _ = authenticated_session_id;
    let work = match serde_json::from_value::<SubmitWork>(payload.clone()) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "finalize_handlers: malformed submit_work payload"
            );
            return false;
        }
    };
    let activity_payload = serde_json::json!({
        "commit_title": work.commit_title,
        "summary": work.summary,
        "files_changed": work.files_changed,
        "remaining_concerns": work.remaining_concerns,
    })
    .to_string();
    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if let Err(e) = repo
        .log_activity(
            Some(task_id),
            "agent-supervisor",
            "worker",
            "work_submitted",
            &activity_payload,
        )
        .await
    {
        tracing::warn!(
            task_id = %task_id,
            error = %e,
            "finalize_handlers: failed to log submit_work activity"
        );
        return false;
    }
    // Attempt lifecycle: advance the matching pending attempt to `submitted`.
    // Best-effort — never fails the submit path.
    crate::attempt_lifecycle::advance_to_submitted(
        app_state,
        crate::attempt_lifecycle::SubmitAdvancementParams {
            task_id,
            role: "worker",
            submit_ref: None,
            checkpoint_ref: None,
            mirror_head_sha: None,
            github_head_sha: None,
            summary: Some(&work.summary),
            summary_json: None,
        },
    )
    .await;
    true
}

/// Apply a reviewer's AC verdicts and log the ordinary review handoff.
pub(crate) async fn handle_submit_review(
    payload: &serde_json::Value,
    task_id: &str,
    app_state: &SlotContext,
) {
    let review = match serde_json::from_value::<SubmitReview>(payload.clone()) {
        Ok(review) => review,
        Err(error) => {
            tracing::warn!(
                task_id = %task_id,
                error = %error,
                "finalize_handlers: malformed submit_review payload"
            );
            return;
        }
    };
    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if !review.acceptance_criteria.is_empty() {
        match repo.get(task_id).await {
            Ok(Some(task)) => {
                let acceptance_criteria =
                    apply_ac_verdicts(&task.acceptance_criteria, &review.acceptance_criteria);
                if let Err(error) = repo
                    .update(
                        task_id,
                        &task.title,
                        &task.description,
                        &task.design,
                        task.priority,
                        &task.owner,
                        &task.labels,
                        &acceptance_criteria,
                    )
                    .await
                {
                    tracing::warn!(
                        task_id = %task_id,
                        error = %error,
                        "finalize_handlers: failed to update AC from submit_review"
                    );
                }
            }
            Ok(None) => tracing::warn!(
                task_id = %task_id,
                "finalize_handlers: task not found for AC update"
            ),
            Err(error) => tracing::warn!(
                task_id = %task_id,
                error = %error,
                "finalize_handlers: failed to load task for AC update"
            ),
        }
    }

    let activity_payload = serde_json::json!({
        "verdict": review.verdict,
        "feedback": review.feedback,
    })
    .to_string();
    if let Err(error) = repo
        .log_activity(
            Some(task_id),
            "agent-supervisor",
            "reviewer",
            "review_submitted",
            &activity_payload,
        )
        .await
    {
        tracing::warn!(
            task_id = %task_id,
            error = %error,
            "finalize_handlers: failed to log submit_review activity"
        );
    }

    if review.verdict == "rejected" {
        record_rejected_submission_fingerprint(
            task_id,
            app_state,
            djinn_core::models::RejectedVerdictKind::ReviewerReject.as_str(),
        )
        .await;
    }
}

/// Record the latest task-run worktree fingerprint for a rejected review.
/// Missing worktrees and empty diffs are legitimate historical cases and do
/// not manufacture integrity records.
async fn record_rejected_submission_fingerprint(
    task_id: &str,
    app_state: &SlotContext,
    verdict_kind: &str,
) {
    let task_run_repo = TaskRunRepository::new(app_state.db.clone());
    let runs = match task_run_repo.list_for_task(task_id).await {
        Ok(runs) => runs,
        Err(error) => {
            tracing::warn!(
                task_id = %task_id,
                error = %error,
                "finalize_handlers: failed to query task runs for rejected fingerprint"
            );
            return;
        }
    };
    let Some((task_run_id, workspace_path)) = runs
        .iter()
        .find_map(|run| Some((run.id.as_str(), run.workspace_path.as_deref()?)))
    else {
        tracing::info!(
            task_id = %task_id,
            verdict_kind,
            "finalize_handlers: no worktree available for rejected fingerprint"
        );
        return;
    };
    let fingerprint =
        match djinn_git::compute_submission_diff_fingerprint(std::path::Path::new(workspace_path))
            .await
        {
            Ok(fingerprint) => fingerprint,
            Err(error) => {
                tracing::warn!(
                    task_id = %task_id,
                    task_run_id,
                    error = %error,
                    "finalize_handlers: failed to compute rejected submission fingerprint"
                );
                return;
            }
        };
    let Some(digest) = fingerprint.fingerprint() else {
        tracing::info!(
            task_id = %task_id,
            task_run_id,
            verdict_kind,
            "finalize_handlers: rejected submission worktree has no diff"
        );
        return;
    };
    record_rejected_integrity_entry(
        task_id,
        app_state,
        verdict_kind,
        None,
        Some(task_run_id),
        digest,
    )
    .await;
}

pub(crate) async fn record_rejected_integrity_entry(
    task_id: &str,
    app_state: &SlotContext,
    verdict_kind: &str,
    review_id: Option<&str>,
    task_run_id: Option<&str>,
    digest: &str,
) {
    let integrity_repo = TaskRejectedSubmissionIntegrityRepository::new(app_state.db.clone());
    let current_streak = integrity_repo
        .latest_no_progress_streak_for_task(task_id)
        .await
        .unwrap_or(0);
    let rejected_at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
    let id = uuid::Uuid::now_v7().to_string();
    let params = RecordTaskRejectedSubmissionParams {
        id: &id,
        task_id,
        task_run_id,
        review_id,
        verdict_kind,
        activity_id: None,
        rejected_at: &rejected_at,
        diff_fingerprint: digest,
        no_progress_streak: current_streak + 1,
    };
    if let Err(e) = integrity_repo.record(params).await {
        tracing::warn!(
            task_id = %task_id,
            verdict_kind = verdict_kind,
            error = %e,
            "finalize_handlers: failed to record rejected submission integrity"
        );
    } else {
        tracing::info!(
            task_id = %task_id,
            task_run_id = ?task_run_id,
            verdict_kind = verdict_kind,
            fingerprint = %digest,
            no_progress_streak = current_streak + 1,
            "finalize_handlers: recorded rejected submission integrity"
        );
    }
}

/// Merge incoming per-criterion verdicts into the task's existing AC JSON.
///
/// Uses index-based matching. If an incoming verdict is missing `criterion` text,
/// the existing criterion text at that index is preserved.
pub fn apply_ac_verdicts(existing_json: &str, verdicts: &[AcVerdict]) -> String {
    let existing: Vec<serde_json::Value> = serde_json::from_str(existing_json).unwrap_or_default();
    let merged: Vec<serde_json::Value> = verdicts
        .iter()
        .enumerate()
        .map(|(i, verdict)| {
            let criterion_text = if !verdict.criterion.is_empty() {
                verdict.criterion.clone()
            } else {
                existing
                    .get(i)
                    .and_then(|e| e.get("criterion"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string()
            };
            serde_json::json!({
                "criterion": criterion_text,
                "met": verdict.met,
            })
        })
        .collect();
    serde_json::to_string(&merged).unwrap_or_else(|_| "[]".to_string())
}

/// Log lead decision as a structured activity entry.
pub(crate) async fn handle_submit_decision(
    payload: &serde_json::Value,
    task_id: &str,
    app_state: &SlotContext,
) {
    let decision = match serde_json::from_value::<SubmitDecision>(payload.clone()) {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "finalize_handlers: malformed submit_decision payload"
            );
            return;
        }
    };
    let activity_payload = serde_json::json!({
        "decision": decision.decision,
        "rationale": decision.rationale,
        "evidence": decision.evidence,
        "directive": decision.directive,
        "verification_command": decision.verification_command,
        "exclude_models": decision.exclude_models,
        "park_dossier": decision.park_dossier,
        "created_tasks": decision.created_tasks,
    })
    .to_string();
    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    if let Err(e) = repo
        .log_activity(
            Some(task_id),
            "agent-supervisor",
            "lead",
            "decision_submitted",
            &activity_payload,
        )
        .await
    {
        tracing::warn!(
            task_id = %task_id,
            error = %e,
            "finalize_handlers: failed to log submit_decision activity"
        );
    }
}

/// Log per-task planning activity entries and durably record any blocker the planner declared.
///
/// `finalize_task_id` is the planning task the session ran on; its epic owns
/// any `blocked_on` edges. Each `tasks_reviewed` entry references a real task
/// by its own `task_id` field.
pub(crate) async fn handle_submit_grooming(
    payload: &serde_json::Value,
    finalize_task_id: &str,
    app_state: &SlotContext,
) {
    let grooming = match serde_json::from_value::<SubmitGrooming>(payload.clone()) {
        Ok(g) => g,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "finalize_handlers: malformed submit_grooming payload"
            );
            return;
        }
    };
    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    for entry in &grooming.tasks_reviewed {
        let activity_payload = serde_json::json!({
            "action": entry.action,
            "changes": entry.changes,
        })
        .to_string();
        if let Err(e) = repo
            .log_activity(
                Some(&entry.task_id),
                "agent-supervisor",
                "planner",
                "planning_entry",
                &activity_payload,
            )
            .await
        {
            tracing::warn!(
                task_id = %entry.task_id,
                error = %e,
                "finalize_handlers: failed to log planning_entry activity"
            );
        }
    }
    // Durably record a planner's "blocked on epic X" conclusion as an epic
    // blocker edge, so the coordinator parks this epic's planning until X
    // closes rather than re-deriving "blocked" via a fresh LLM session on every
    // stale-sweep (epic `mygq`, 2026-07-01). Idempotent: `add_blocker` no-ops
    // on a duplicate edge and rejects self-loops / cycles.
    if !grooming.blocked_on.is_empty() {
        record_declared_epic_blockers(finalize_task_id, &grooming.blocked_on, app_state).await;
    }
}

/// Resolve the planning task's epic and wire each declared blocker as an
/// epic-blocker edge. Best-effort: unresolvable refs are logged and skipped;
/// a lookup/DB error never crashes the finalize path.
async fn record_declared_epic_blockers(
    finalize_task_id: &str,
    blocked_on: &[String],
    app_state: &SlotContext,
) {
    let task_repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    let task = match task_repo.get(finalize_task_id).await {
        Ok(Some(t)) => t,
        Ok(None) => {
            tracing::warn!(
                task_id = %finalize_task_id,
                "finalize_handlers: submit_grooming blocked_on — planning task not found"
            );
            return;
        }
        Err(e) => {
            tracing::warn!(
                task_id = %finalize_task_id,
                error = %e,
                "finalize_handlers: submit_grooming blocked_on — failed to load planning task"
            );
            return;
        }
    };
    let Some(epic_id) = task.epic_id.as_deref() else {
        tracing::warn!(
            task_id = %finalize_task_id,
            "finalize_handlers: submit_grooming blocked_on — planning task has no epic; ignoring"
        );
        return;
    };
    let epic_repo =
        djinn_db::EpicRepository::new(app_state.db.clone(), app_state.event_bus.clone());
    for blocker_ref in blocked_on {
        let blocker_ref = blocker_ref.trim();
        if blocker_ref.is_empty() {
            continue;
        }
        let blocking = match epic_repo
            .resolve_in_project(&task.project_id, blocker_ref)
            .await
        {
            Ok(Some(e)) => e,
            Ok(None) => {
                tracing::warn!(
                    epic_id,
                    blocker_ref,
                    "finalize_handlers: submit_grooming blocked_on — no epic matches ref in \
                     project; skipping (reference the blocking EPIC, not a task)"
                );
                continue;
            }
            Err(e) => {
                tracing::warn!(
                    epic_id,
                    blocker_ref,
                    error = %e,
                    "finalize_handlers: submit_grooming blocked_on — epic resolve failed"
                );
                continue;
            }
        };
        match epic_repo.add_blocker(epic_id, &blocking.id).await {
            Ok(()) => tracing::info!(
                epic_id,
                blocking_epic_id = %blocking.id,
                blocking_short_id = %blocking.short_id,
                "finalize_handlers: parked epic on planner-declared blocker"
            ),
            Err(e) => tracing::warn!(
                epic_id,
                blocking_epic_id = %blocking.id,
                error = %e,
                "finalize_handlers: submit_grooming blocked_on — add_blocker failed \
                 (self-loop or cycle?)"
            ),
        }
    }
}
