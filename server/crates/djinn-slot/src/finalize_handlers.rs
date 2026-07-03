use crate::finalize_types::{
    AcVerdict, AutoSubmitReviewMetadataPayload, SubmitDecision, SubmitGrooming, SubmitReview,
    SubmitWork,
};
use crate::host::SlotContext;
use djinn_core::events::DjinnEventEnvelope;
use djinn_db::TaskRepository;
use djinn_db::repositories::task_run::TaskRunRepository;
use djinn_db::repositories::verify_run::{
    AutoSubmitReviewRepository, CreateAutoSubmitReviewParams, RecordTaskRejectedSubmissionParams,
    TaskRejectedSubmissionIntegrityRepository,
};
use djinn_git::{SubmissionDiffFingerprint, compute_submission_diff_fingerprint};

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
    app_state: &SlotContext,
) {
    let _ = process_finalize_payload_with_outcome(payload, finalize_tool_name, task_id, app_state)
        .await;
}

pub async fn process_finalize_payload_with_outcome(
    payload: &Option<serde_json::Value>,
    finalize_tool_name: &str,
    task_id: &str,
    app_state: &SlotContext,
) -> bool {
    let Some(payload) = payload else { return true };

    match finalize_tool_name {
        "submit_work" => handle_submit_work(payload, task_id, app_state, true).await,
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

pub async fn process_auto_submit_payload(
    payload: &serde_json::Value,
    task_id: &str,
    app_state: &SlotContext,
) -> bool {
    handle_submit_work(payload, task_id, app_state, false).await
}

/// Persist a budget-park handoff summary using the same payload shape as
/// `submit_work`, so `extract_worker_context` can surface it unchanged.
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
            "work_submitted",
            &activity_payload,
        )
        .await
    {
        tracing::warn!(
            task_id = %task_id,
            error = %e,
            "finalize_handlers: failed to log budget-park work_submitted activity"
        );
    }
}

/// Log structured work-submission activity for a worker session.
async fn handle_submit_work(
    payload: &serde_json::Value,
    task_id: &str,
    app_state: &SlotContext,
    model_called_submit_work: bool,
) -> bool {
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
    let metadata = work.auto_submit_review_metadata.clone();

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

    if let Some(metadata) = metadata {
        match resolve_auto_submit_diff_fingerprint(task_id, &metadata, app_state).await {
            Ok(fingerprint) => {
                let mut metadata = metadata;
                if let Some(fingerprint) = fingerprint {
                    metadata.diff_fingerprint = fingerprint;
                } else {
                    metadata.diff_fingerprint.clear();
                }
                return persist_auto_submit_review_metadata(
                    metadata,
                    app_state,
                    model_called_submit_work,
                    task_id,
                )
                .await;
            }
            Err(()) => {
                // resolve_auto_submit_diff_fingerprint already emitted a reason-specific
                // `submission_fingerprint_unavailable` event for the unavailable/no-diff
                // paths; avoid double-emitting a misleading `compute_error` here.
                return persist_auto_submit_review_metadata(
                    metadata,
                    app_state,
                    model_called_submit_work,
                    task_id,
                )
                .await;
            }
        }
    }

    true
}

/// Resolve the complete submission diff fingerprint for the worktree associated with an
/// accepted auto-submit payload. Returns `Ok(Some(digest))` when the worktree is available and
/// has a non-empty diff, `Ok(None)` when the worktree is available but has no diff, and `Err(())`
/// when the worktree path is unavailable or the helper fails. The caller should keep existing
/// submit behavior safe in all error/no-diff cases and never fabricate a fingerprint.
async fn resolve_auto_submit_diff_fingerprint(
    task_id: &str,
    metadata: &AutoSubmitReviewMetadataPayload,
    app_state: &SlotContext,
) -> Result<Option<String>, ()> {
    let worktree_path =
        match resolve_task_worktree_path(task_id, &metadata.task_run_id, app_state).await {
            Some(p) => p,
            None => {
                emit_fingerprint_unavailable_event(
                    task_id,
                    &metadata.task_run_id,
                    "workspace_unavailable",
                    app_state,
                );
                return Err(());
            }
        };

    match compute_submission_diff_fingerprint(&worktree_path).await {
        Ok(SubmissionDiffFingerprint::Diff(digest)) => {
            tracing::info!(
                task_id = %task_id,
                worktree_path = %worktree_path.display(),
                fingerprint_len = digest.fingerprint.len(),
                changed_paths = ?digest.changed_paths,
                "auto-submit: computed complete submission diff fingerprint"
            );
            Ok(Some(digest.fingerprint))
        }
        Ok(SubmissionDiffFingerprint::NoDiff(no_diff)) => {
            emit_fingerprint_unavailable_event(
                task_id,
                &metadata.task_run_id,
                "no_diff",
                app_state,
            );
            tracing::info!(
                task_id = %task_id,
                worktree_path = %worktree_path.display(),
                merge_base = ?no_diff.merge_base,
                "auto-submit: no diff in worktree for fingerprint"
            );
            Ok(None)
        }
        Err(e) => {
            emit_fingerprint_unavailable_event(
                task_id,
                &metadata.task_run_id,
                "compute_error",
                app_state,
            );
            tracing::warn!(
                task_id = %task_id,
                worktree_path = %worktree_path.display(),
                error = %e,
                "auto-submit: failed to compute submission diff fingerprint"
            );
            Err(())
        }
    }
}

/// Resolve the task worktree path for an accepted auto-submit payload.
///
/// First checks the slot context `working_root` (the active task worktree),
/// then falls back to the task run's recorded `workspace_path`.
async fn resolve_task_worktree_path(
    task_id: &str,
    task_run_id: &str,
    app_state: &SlotContext,
) -> Option<std::path::PathBuf> {
    if let Some(root) = app_state.working_root.as_ref() {
        let root = root.to_path_buf();
        if root.exists() {
            return Some(root);
        }
    }

    let task_run_repo =
        djinn_db::repositories::task_run::TaskRunRepository::new(app_state.db.clone());
    let run = match task_run_repo.get(task_run_id).await {
        Ok(Some(run)) => run,
        Ok(None) => {
            tracing::warn!(
                task_id = %task_id,
                task_run_id = %task_run_id,
                "auto-submit: task_run not found for worktree lookup"
            );
            return None;
        }
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                task_run_id = %task_run_id,
                error = %e,
                "auto-submit: failed to load task_run for worktree lookup"
            );
            return None;
        }
    };

    let workspace_path = match run.workspace_path.filter(|p| !p.is_empty()) {
        Some(p) => p,
        None => {
            tracing::warn!(
                task_id = %task_id,
                task_run_id = %task_run_id,
                "auto-submit: no workspace_path available for worktree lookup"
            );
            return None;
        }
    };

    let path = std::path::PathBuf::from(&workspace_path);
    if !path.exists() {
        tracing::warn!(
            task_id = %task_id,
            workspace_path = %workspace_path,
            "auto-submit: workspace_path does not exist; skipping worktree lookup"
        );
        return None;
    }

    Some(path)
}

fn emit_fingerprint_unavailable_event(
    task_id: &str,
    task_run_id: &str,
    reason: &str,
    ctx: &SlotContext,
) {
    ctx.event_bus.send(DjinnEventEnvelope {
        entity_type: "verify",
        action: "submission_fingerprint_unavailable",
        payload: serde_json::json!({
            "task_id": task_id,
            "task_run_id": task_run_id,
            "reason": reason.to_string(),
        }),
        id: Some(task_id.to_string()),
        project_id: None,
        from_sync: false,
    });
}

async fn persist_auto_submit_review_metadata(
    metadata: AutoSubmitReviewMetadataPayload,
    app_state: &SlotContext,
    model_called_submit_work: bool,
    task_id: &str,
) -> bool {
    let repo = AutoSubmitReviewRepository::new(app_state.db.clone());
    let id = uuid::Uuid::now_v7().to_string();
    let result = repo
        .create(CreateAutoSubmitReviewParams {
            id: &id,
            task_run_id: &metadata.task_run_id,
            trigger_reason: &metadata.trigger_reason,
            diff_fingerprint: &metadata.diff_fingerprint,
            verify_source: metadata.verify_source.as_deref(),
            verify_run_id: metadata.verify_run_id.as_deref(),
            verify_timestamp: metadata.verify_timestamp.as_deref(),
            session_id: metadata.session_id.as_deref(),
            model_id: metadata.model_id.as_deref(),
            no_progress_streak: metadata.no_progress_streak,
            model_called_submit_work,
        })
        .await;

    if let Err(e) = result {
        tracing::warn!(
            task_id = %task_id,
            task_run_id = %metadata.task_run_id,
            error = %e,
            "finalize_handlers: failed to persist auto-submit review metadata"
        );
        return false;
    }

    true
}

/// Atomically set AC met/unmet on the task from the criteria array, then log the verdict.
async fn handle_submit_review(payload: &serde_json::Value, task_id: &str, app_state: &SlotContext) {
    let review = match serde_json::from_value::<SubmitReview>(payload.clone()) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "finalize_handlers: malformed submit_review payload"
            );
            return;
        }
    };

    let repo = TaskRepository::new(app_state.db.clone(), app_state.event_bus.clone());

    // Atomically set AC met/unmet state from the criteria array.
    if !review.acceptance_criteria.is_empty() {
        match repo.get(task_id).await {
            Ok(Some(task)) => {
                let ac_json =
                    apply_ac_verdicts(&task.acceptance_criteria, &review.acceptance_criteria);
                if let Err(e) = repo
                    .update(
                        task_id,
                        &task.title,
                        &task.description,
                        &task.design,
                        task.priority,
                        &task.owner,
                        &task.labels,
                        &ac_json,
                    )
                    .await
                {
                    tracing::warn!(
                        task_id = %task_id,
                        error = %e,
                        "finalize_handlers: failed to update AC from submit_review"
                    );
                }
            }
            Ok(None) => {
                tracing::warn!(
                    task_id = %task_id,
                    "finalize_handlers: task not found for AC update"
                );
            }
            Err(e) => {
                tracing::warn!(
                    task_id = %task_id,
                    error = %e,
                    "finalize_handlers: failed to load task for AC update"
                );
            }
        }
    }

    // Log verdict and feedback as structured activity.
    let activity_payload = serde_json::json!({
        "verdict": review.verdict,
        "feedback": review.feedback,
    })
    .to_string();

    if let Err(e) = repo
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
            error = %e,
            "finalize_handlers: failed to log submit_review activity"
        );
    }

    // When the reviewer verdict is "rejected", persist a task-level rejected
    // submission fingerprint so the live submit-work guard can detect
    // no-progress resubmissions in future task runs.
    if review.verdict == "rejected" {
        record_rejected_submission_fingerprint(
            task_id,
            app_state,
            djinn_core::models::RejectedVerdictKind::ReviewerReject.as_str(),
            None,
        )
        .await;
    }
}

/// Attempt to compute the submission diff fingerprint from the task's latest
/// worktree and record it as a task-level rejected submission integrity entry.
///
/// If the worktree is unavailable (historical run, deleted workspace, or
/// no worktree assigned), or if the worktree has no diff (empty submission),
/// persistence is skipped and a structured log is emitted instead. This
/// follows the "no fake fingerprints" design invariant from epic 8k7q.
pub(crate) async fn record_rejected_submission_fingerprint(
    task_id: &str,
    app_state: &SlotContext,
    verdict_kind: &str,
    review_id: Option<&str>,
) {
    let task_run_repo = TaskRunRepository::new(app_state.db.clone());
    let runs = match task_run_repo.list_for_task(task_id).await {
        Ok(runs) => runs,
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                error = %e,
                "finalize_handlers: failed to query task runs for rejected fingerprint"
            );
            return;
        }
    };

    // Find the latest run with a workspace_path.
    let Some((task_run_id, workspace_path)) = runs
        .iter()
        .find(|r| r.workspace_path.is_some())
        .and_then(|r| Some((r.id.clone(), r.workspace_path.clone()?)))
    else {
        tracing::info!(
            task_id = %task_id,
            verdict_kind = verdict_kind,
            "finalize_handlers: no worktree available for rejected submission \
             fingerprint; skipping persistence (historical/no-worktree case)"
        );
        return;
    };

    let worktree = std::path::PathBuf::from(&workspace_path);
    let fingerprint = match djinn_git::compute_submission_diff_fingerprint(&worktree).await {
        Ok(fp) => fp,
        Err(e) => {
            tracing::warn!(
                task_id = %task_id,
                task_run_id = %task_run_id,
                worktree = %workspace_path,
                error = %e,
                "finalize_handlers: failed to compute submission diff fingerprint \
                 for rejected submission; skipping persistence"
            );
            return;
        }
    };

    let Some(digest) = fingerprint.fingerprint().map(|s| s.to_string()) else {
        tracing::info!(
            task_id = %task_id,
            task_run_id = %task_run_id,
            verdict_kind = verdict_kind,
            "finalize_handlers: rejected submission worktree has no diff \
             (NoDiff); skipping rejected fingerprint persistence"
        );
        return;
    };

    record_rejected_integrity_entry(
        task_id,
        app_state,
        verdict_kind,
        review_id,
        Some(&task_run_id),
        &digest,
    )
    .await;
}

/// Persist a task-level rejected submission integrity row.
///
/// Shared by review-rejection, settlement-rejection, and PR-change-requested
/// paths. Increments the task-level `no_progress_streak` by 1 over the
/// current latest value (defaulting to 0).
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
async fn handle_submit_decision(
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

/// Log per-task planning activity entries and durably record any blocker the
/// planner declared.
///
/// `finalize_task_id` is the planning task the session ran on; its epic owns
/// any `blocked_on` edges. Each `tasks_reviewed` entry references a real task
/// by its own `task_id` field.
async fn handle_submit_grooming(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers;

    // ─── apply_ac_verdicts ────────────────────────────────────────────────────

    #[test]
    fn apply_ac_verdicts_sets_met_flags_from_payload() {
        let existing =
            r#"[{"criterion":"write tests","met":false},{"criterion":"passing ci","met":false}]"#;
        let verdicts = vec![
            AcVerdict {
                criterion: "write tests".to_string(),
                met: true,
            },
            AcVerdict {
                criterion: "passing ci".to_string(),
                met: true,
            },
        ];
        let result = apply_ac_verdicts(existing, &verdicts);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed[0]["met"], true);
        assert_eq!(parsed[1]["met"], true);
    }

    #[test]
    fn apply_ac_verdicts_preserves_existing_criterion_text_when_empty() {
        let existing = r#"[{"criterion":"write tests","met":false}]"#;
        let verdicts = vec![AcVerdict {
            criterion: String::new(),
            met: true,
        }];
        let result = apply_ac_verdicts(existing, &verdicts);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed[0]["criterion"], "write tests");
        assert_eq!(parsed[0]["met"], true);
    }

    #[test]
    fn apply_ac_verdicts_handles_empty_existing_gracefully() {
        let existing = "not-valid-json";
        let verdicts = vec![AcVerdict {
            criterion: "x".to_string(),
            met: false,
        }];
        let result = apply_ac_verdicts(existing, &verdicts);
        let parsed: Vec<serde_json::Value> = serde_json::from_str(&result).unwrap();
        assert_eq!(parsed[0]["criterion"], "x");
        assert_eq!(parsed[0]["met"], false);
    }

    // ─── process_finalize_payload: submit_work ────────────────────────────────

    #[tokio::test]
    async fn budget_park_logs_extractor_compatible_work_submitted() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        handle_budget_park(
            "completed A; B remains",
            "budget-triggered wind-down summary captured",
            &task.id,
            &ctx,
        )
        .await;

        let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
        let entries = repo.list_activity(&task.id).await.unwrap();
        let work_entries: Vec<_> = entries
            .iter()
            .filter(|e| e.event_type == "work_submitted")
            .collect();
        assert_eq!(work_entries.len(), 1);

        let body: serde_json::Value = serde_json::from_str(&work_entries[0].payload).unwrap();
        assert_eq!(body["summary"], "completed A; B remains");
        assert_eq!(
            body["remaining_concerns"],
            "budget-parked: budget-triggered wind-down summary captured"
        );
    }

    #[tokio::test]
    async fn budget_park_empty_summary_skips_activity() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        handle_budget_park("   ", "ignored", &task.id, &ctx).await;

        let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
        let entries = repo.list_activity(&task.id).await.unwrap();
        assert!(entries.iter().all(|e| e.event_type != "work_submitted"));
    }

    #[tokio::test]
    async fn submit_work_logs_activity_with_summary_and_files() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        let payload = Some(serde_json::json!({
            "task_id": task.short_id,
            "commit_title": "feat: implement the feature",
            "summary": "implemented the feature",
            "files_changed": ["src/main.rs", "src/lib.rs"],
            "remaining_concerns": ["needs perf testing"]
        }));

        process_finalize_payload(&payload, "submit_work", &task.id, &ctx).await;

        let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
        let entries = repo.list_activity(&task.id).await.unwrap();
        let work_entry = entries.iter().find(|e| e.event_type == "work_submitted");
        assert!(
            work_entry.is_some(),
            "expected work_submitted activity entry"
        );

        let body: serde_json::Value = serde_json::from_str(&work_entry.unwrap().payload).unwrap();
        assert_eq!(body["summary"], "implemented the feature");
        assert_eq!(body["files_changed"][0], "src/main.rs");
        assert_eq!(body["remaining_concerns"][0], "needs perf testing");
    }

    #[tokio::test]
    async fn budget_park_submit_work_activity_surfaces_unchanged() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        let payload = Some(serde_json::json!({
            "task_id": task.short_id,
            "commit_title": "park budget summary",
            "summary": "finished the safe subset before parking",
            "files_changed": ["src/lib.rs"],
            "remaining_concerns": ["budget-parked: finish the follow-up UI snapshot"]
        }));

        process_finalize_payload(&payload, "submit_work", &task.id, &ctx).await;

        let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
        let entries = repo.list_activity(&task.id).await.unwrap();
        let work_entry = entries
            .iter()
            .find(|entry| entry.event_type == "work_submitted")
            .expect("expected budget-park work_submitted activity entry");
        let body: serde_json::Value = serde_json::from_str(&work_entry.payload).unwrap();
        assert_eq!(body["summary"], "finished the safe subset before parking");
        assert_eq!(
            body["remaining_concerns"][0],
            "budget-parked: finish the follow-up UI snapshot"
        );
    }

    #[tokio::test]
    async fn submit_work_malformed_payload_does_not_crash() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        // Missing required "summary" field.
        let payload = Some(serde_json::json!({"task_id": task.id}));
        // Should not panic.
        process_finalize_payload(&payload, "submit_work", &task.id, &ctx).await;
    }

    // ─── process_finalize_payload: submit_review ──────────────────────────────

    #[tokio::test]
    async fn submit_review_atomically_sets_ac_from_criteria_array() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        // Seed AC with met=false.
        TaskRepository::new(db.clone(), ctx.event_bus.clone())
            .set_acceptance_criteria(
                &task.id,
                r#"[{"criterion":"write tests","met":false},{"criterion":"passes ci","met":false}]"#,
            )
            .await
            .unwrap();

        let payload = Some(serde_json::json!({
            "task_id": task.id,
            "verdict": "approved",
            "acceptance_criteria": [
                {"criterion": "write tests", "met": true},
                {"criterion": "passes ci", "met": true}
            ],
            "feedback": null
        }));

        process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;

        // AC should be updated in the DB.
        let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
        let updated = repo.get(&task.id).await.unwrap().unwrap();
        let ac: Vec<serde_json::Value> =
            serde_json::from_str(&updated.acceptance_criteria).unwrap();
        assert_eq!(ac[0]["met"], true);
        assert_eq!(ac[1]["met"], true);
    }

    #[tokio::test]
    async fn submit_review_logs_verdict_activity() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        let payload = Some(serde_json::json!({
            "task_id": task.id,
            "verdict": "rejected",
            "acceptance_criteria": [],
            "feedback": "missing edge case handling"
        }));

        process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;

        let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
        let entries = repo.list_activity(&task.id).await.unwrap();
        let entry = entries.iter().find(|e| e.event_type == "review_submitted");
        assert!(entry.is_some(), "expected review_submitted activity entry");

        let body: serde_json::Value = serde_json::from_str(&entry.unwrap().payload).unwrap();
        assert_eq!(body["verdict"], "rejected");
        assert_eq!(body["feedback"], "missing edge case handling");
    }

    #[tokio::test]
    async fn submit_review_malformed_payload_does_not_crash() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        // "verdict" is required but missing.
        let payload = Some(serde_json::json!({"task_id": task.id}));
        process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;
    }

    // ─── process_finalize_payload: submit_decision ────────────────────────────

    #[tokio::test]
    async fn submit_decision_logs_decision_activity() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        let payload = Some(serde_json::json!({
            "task_id": task.id,
            "decision": "reopen",
            "rationale": "scope was too broad",
            "created_tasks": []
        }));

        process_finalize_payload(&payload, "submit_decision", &task.id, &ctx).await;

        let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
        let entries = repo.list_activity(&task.id).await.unwrap();
        let entry = entries
            .iter()
            .find(|e| e.event_type == "decision_submitted");
        assert!(
            entry.is_some(),
            "expected decision_submitted activity entry"
        );

        let body: serde_json::Value = serde_json::from_str(&entry.unwrap().payload).unwrap();
        assert_eq!(body["decision"], "reopen");
        assert_eq!(body["rationale"], "scope was too broad");
    }

    #[tokio::test]
    async fn submit_decision_malformed_payload_does_not_crash() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        // "decision" is required but missing.
        let payload = Some(serde_json::json!({"task_id": task.id}));
        process_finalize_payload(&payload, "submit_decision", &task.id, &ctx).await;
    }

    // ─── process_finalize_payload: submit_grooming ────────────────────────────

    #[tokio::test]
    async fn submit_grooming_logs_per_task_activity_entries() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task1 = test_helpers::create_test_task(&db, &project.id, &epic.id).await;
        let task2 = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        let payload = Some(serde_json::json!({
            "tasks_reviewed": [
                {"task_id": task1.id, "action": "promoted", "changes": "bumped priority to 1"},
                {"task_id": task2.id, "action": "skipped", "changes": null}
            ],
            "summary": "groomed 2 tasks"
        }));

        // Planner is project-scoped; pass synthetic task_id.
        let synthetic_id = format!("project:{}:planner", project.id);
        process_finalize_payload(&payload, "submit_grooming", &synthetic_id, &ctx).await;

        let repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());

        let entries1 = repo.list_activity(&task1.id).await.unwrap();
        let e1 = entries1.iter().find(|e| e.event_type == "planning_entry");
        assert!(e1.is_some(), "expected planning_entry for task1");
        let b1: serde_json::Value = serde_json::from_str(&e1.unwrap().payload).unwrap();
        assert_eq!(b1["action"], "promoted");
        assert_eq!(b1["changes"], "bumped priority to 1");

        let entries2 = repo.list_activity(&task2.id).await.unwrap();
        let e2 = entries2.iter().find(|e| e.event_type == "planning_entry");
        assert!(e2.is_some(), "expected planning_entry for task2");
        let b2: serde_json::Value = serde_json::from_str(&e2.unwrap().payload).unwrap();
        assert_eq!(b2["action"], "skipped");
    }

    /// A planner concluding "blocked on epic X, no tasks created" must durably
    /// record the epic-blocker edge, so the coordinator parks this epic instead
    /// of re-planning every stale-sweep (epic `mygq`, 2026-07-01).
    #[tokio::test]
    async fn submit_grooming_blocked_on_records_epic_blocker_durably() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let parked = test_helpers::create_test_epic(&db, &project.id).await;
        let blocker = test_helpers::create_test_epic(&db, &project.id).await;
        // The planning session runs on a real planning task under the parked epic.
        let planning_task = test_helpers::create_test_task(&db, &project.id, &parked.id).await;

        let epic_repo = djinn_db::EpicRepository::new(db.clone(), ctx.event_bus.clone());
        assert!(
            !epic_repo.has_unresolved_blockers(&parked.id).await.unwrap(),
            "precondition: parked epic starts with no blockers"
        );

        // Declare the blocker by short_id and create no tasks.
        let payload = Some(serde_json::json!({
            "tasks_reviewed": [],
            "summary": "blocked on foundation epic; no work created",
            "decision": "escalate",
            "blocked_on": [blocker.short_id],
        }));
        process_finalize_payload(&payload, "submit_grooming", &planning_task.id, &ctx).await;

        // The durable edge must exist and the gate must see an open blocker.
        assert!(
            epic_repo.has_unresolved_blockers(&parked.id).await.unwrap(),
            "blocked_on must durably record an epic-blocker edge"
        );
        let blockers = epic_repo.list_blockers(&parked.id).await.unwrap();
        assert_eq!(blockers.len(), 1);
        assert_eq!(blockers[0].epic_id, blocker.id);

        // Idempotent: re-declaring the same blocker must not error or duplicate.
        process_finalize_payload(&payload, "submit_grooming", &planning_task.id, &ctx).await;
        let blockers_again = epic_repo.list_blockers(&parked.id).await.unwrap();
        assert_eq!(
            blockers_again.len(),
            1,
            "re-declaring the same blocker must be idempotent"
        );

        // Closing the blocker clears the gate (event-driven wake path).
        epic_repo.close(&blocker.id).await.unwrap();
        assert!(
            !epic_repo.has_unresolved_blockers(&parked.id).await.unwrap(),
            "closing the blocker must clear the park gate"
        );
    }

    /// An unresolvable `blocked_on` ref (e.g. a task short_id, not an epic) must
    /// be skipped without crashing and without recording a bogus edge.
    #[tokio::test]
    async fn submit_grooming_blocked_on_unresolvable_ref_is_skipped() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let parked = test_helpers::create_test_epic(&db, &project.id).await;
        let planning_task = test_helpers::create_test_task(&db, &project.id, &parked.id).await;

        let payload = Some(serde_json::json!({
            "tasks_reviewed": [],
            "blocked_on": ["does-not-exist"],
        }));
        process_finalize_payload(&payload, "submit_grooming", &planning_task.id, &ctx).await;

        let epic_repo = djinn_db::EpicRepository::new(db.clone(), ctx.event_bus.clone());
        assert!(
            !epic_repo.has_unresolved_blockers(&parked.id).await.unwrap(),
            "unresolvable blocked_on ref must not record a blocker"
        );
    }

    #[tokio::test]
    async fn submit_grooming_malformed_payload_does_not_crash() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );

        // tasks_reviewed items missing required "action" field — SubmitGrooming itself
        // has tasks_reviewed as #[serde(default)] Vec, so malformed items are the issue.
        // Since tasks_reviewed has #[serde(default)], this will succeed with empty vec.
        // Test a completely invalid payload type instead.
        let payload = Some(serde_json::json!("not-an-object"));
        process_finalize_payload(&payload, "submit_grooming", "project:x:planner", &ctx).await;
    }

    // ─── no-op cases ──────────────────────────────────────────────────────────

    #[tokio::test]
    async fn none_payload_is_a_noop() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        // Should not panic or error.
        process_finalize_payload(&None, "submit_work", "any-task-id", &ctx).await;
    }

    #[tokio::test]
    async fn unknown_finalize_tool_is_a_noop() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let payload = Some(serde_json::json!({"anything": "here"}));
        process_finalize_payload(&payload, "submit_unknown", "any-task-id", &ctx).await;
    }

    // ─── auto-submit review metadata persistence ────────────────────────────

    #[tokio::test]
    async fn submit_work_with_auto_submit_metadata_records_model_called_true() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        // Create a task_run so the metadata can reference it.
        let run_id = uuid::Uuid::now_v7().to_string();
        djinn_db::repositories::task_run::TaskRunRepository::new(db.clone())
            .create(djinn_db::repositories::task_run::CreateTaskRunParams {
                id: &run_id,
                project_id: &project.id,
                task_id: &task.id,
                trigger_type: djinn_core::models::TaskRunTrigger::NewTask.as_str(),
                status: None,
                workspace_path: None,
                mirror_ref: None,
            })
            .await
            .expect("create task run");

        let payload = Some(serde_json::json!({
            "task_id": task.short_id,
            "commit_title": "feat: model submitted",
            "summary": "model called submit_work with review metadata",
            "files_changed": ["src/main.rs"],
            "remaining_concerns": [],
            "auto_submit_review_metadata": {
                "task_run_id": run_id,
                "trigger_reason": "idle",
                "diff_fingerprint": "abc123",
                "verify_source": "ci",
                "verify_run_id": "ci-42",
                "verify_timestamp": "2026-07-01T10:00:00.000Z",
                "session_id": "sess-1",
                "model_id": "model-1",
                "no_progress_streak": 2
            }
        }));

        // Called via process_finalize_payload_with_outcome — this is the normal
        // model-called submit_work path. The `model_called_submit_work` flag
        // should be `true` in the persisted record.
        let ok =
            process_finalize_payload_with_outcome(&payload, "submit_work", &task.id, &ctx).await;
        assert!(ok);

        // work_submitted activity should be logged.
        let task_repo = TaskRepository::new(db.clone(), ctx.event_bus.clone());
        let entries = task_repo.list_activity(&task.id).await.unwrap();
        assert!(entries.iter().any(|e| e.event_type == "work_submitted"));

        // Auto-submit review record should be persisted with model_called=true.
        let records = djinn_db::repositories::verify_run::AutoSubmitReviewRepository::new(db)
            .list_for_task_run(&run_id)
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert!(records[0].model_called_submit_work);
        assert_eq!(records[0].trigger_reason, "idle");
        assert_eq!(records[0].diff_fingerprint, "abc123");
        assert_eq!(records[0].verify_source.as_deref(), Some("ci"));
        assert_eq!(records[0].verify_run_id.as_deref(), Some("ci-42"));
        assert_eq!(
            records[0].verify_timestamp.as_deref(),
            Some("2026-07-01T10:00:00.000Z")
        );
        assert_eq!(records[0].session_id.as_deref(), Some("sess-1"));
        assert_eq!(records[0].model_id.as_deref(), Some("model-1"));
        assert_eq!(records[0].no_progress_streak, 2);
    }

    #[tokio::test]
    async fn auto_submit_payload_records_model_called_false() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        let run_id = uuid::Uuid::now_v7().to_string();
        djinn_db::repositories::task_run::TaskRunRepository::new(db.clone())
            .create(djinn_db::repositories::task_run::CreateTaskRunParams {
                id: &run_id,
                project_id: &project.id,
                task_id: &task.id,
                trigger_type: djinn_core::models::TaskRunTrigger::NewTask.as_str(),
                status: None,
                workspace_path: None,
                mirror_ref: None,
            })
            .await
            .expect("create task run");

        let payload = serde_json::json!({
            "task_id": task.short_id,
            "commit_title": "auto-submit verified worker diff",
            "summary": "Auto-submitted eligible green exact diff.",
            "files_changed": ["src/lib.rs"],
            "remaining_concerns": [],
            "auto_submit_review_metadata": {
                "task_run_id": run_id,
                "trigger_reason": "controlled_termination",
                "diff_fingerprint": "diff-789",
                "verify_source": "worker",
                "verify_run_id": "worker-run-5",
                "verify_timestamp": "2026-07-02T08:00:00.000Z",
                "session_id": "sess-5",
                "model_id": "model-5",
                "no_progress_streak": 4
            }
        });

        // Called via process_auto_submit_payload — this is the auto-submit
        // path. The `model_called_submit_work` flag should be `false`.
        let ok = process_auto_submit_payload(&payload, &task.id, &ctx).await;
        assert!(ok);

        let records = djinn_db::repositories::verify_run::AutoSubmitReviewRepository::new(db)
            .list_for_task_run(&run_id)
            .await
            .unwrap();
        assert_eq!(records.len(), 1);
        assert!(!records[0].model_called_submit_work);
        assert_eq!(records[0].trigger_reason, "controlled_termination");
        assert_eq!(records[0].diff_fingerprint, "diff-789");
        assert_eq!(records[0].verify_source.as_deref(), Some("worker"));
        assert_eq!(records[0].verify_run_id.as_deref(), Some("worker-run-5"));
        assert_eq!(
            records[0].verify_timestamp.as_deref(),
            Some("2026-07-02T08:00:00.000Z")
        );
        assert_eq!(records[0].session_id.as_deref(), Some("sess-5"));
        assert_eq!(records[0].model_id.as_deref(), Some("model-5"));
        assert_eq!(records[0].no_progress_streak, 4);
    }

    // ── rejected submission fingerprint persistence ────────────────────────

    /// Helper: create a git repo with an initial commit, write a dirty file,
    /// and return the tempdir (kept alive for the test duration).
    fn init_git_repo_with_dirty_file() -> tempfile::TempDir {
        let dir = tempfile::Builder::new()
            .prefix("djinn-test-git-")
            .tempdir()
            .expect("create temp dir");

        let run_git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("run git");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };

        run_git(&["init"]);
        run_git(&["config", "--local", "user.email", "test@test.com"]);
        run_git(&["config", "--local", "user.name", "Test User"]);
        run_git(&["config", "--local", "commit.gpgsign", "false"]);

        std::fs::write(dir.path().join("README.md"), "hello\n").expect("write readme");
        run_git(&["add", "README.md"]);
        run_git(&["commit", "-m", "init"]);
        run_git(&["branch", "-m", "main"]);

        // Make a dirty tracked edit so the fingerprint computes a Diff.
        std::fs::write(dir.path().join("README.md"), "hello\ndirty\n").expect("write dirty");

        dir
    }

    /// Helper: create a task_run with a specific workspace_path.
    async fn create_run_with_workspace(
        db: &djinn_db::Database,
        project_id: &str,
        task_id: &str,
        workspace_path: Option<&str>,
    ) -> String {
        let id = uuid::Uuid::now_v7().to_string();
        djinn_db::repositories::task_run::TaskRunRepository::new(db.clone())
            .create(djinn_db::repositories::task_run::CreateTaskRunParams {
                id: &id,
                project_id,
                task_id,
                trigger_type: djinn_core::models::TaskRunTrigger::NewTask.as_str(),
                status: None,
                workspace_path,
                mirror_ref: None,
            })
            .await
            .expect("create task run");
        id
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejected_review_records_fingerprint_when_worktree_has_diff() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        let worktree = init_git_repo_with_dirty_file();
        let _run_id = create_run_with_workspace(
            &db,
            &project.id,
            &task.id,
            Some(worktree.path().to_str().unwrap()),
        )
        .await;

        let payload = Some(serde_json::json!({
            "task_id": task.id,
            "verdict": "rejected",
            "acceptance_criteria": [],
            "feedback": "missing edge case handling"
        }));

        process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;

        let integrity_repo =
            djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository::new(db);
        let latest = integrity_repo
            .latest_for_task(&task.id)
            .await
            .unwrap()
            .expect("expected rejected integrity record after rejected review");

        assert_eq!(
            latest.verdict_kind,
            djinn_core::models::RejectedVerdictKind::ReviewerReject.as_str()
        );
        assert!(!latest.diff_fingerprint.is_empty());
        assert_eq!(latest.no_progress_streak, 1);
        assert!(latest.task_run_id.is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejected_review_skips_persistence_when_worktree_is_nodiff() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        // Create a clean git repo with no dirty changes.
        let dir = tempfile::Builder::new()
            .prefix("djinn-test-nodiff-")
            .tempdir()
            .expect("create temp dir");
        let run_git = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .expect("run git");
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run_git(&["init"]);
        run_git(&["config", "--local", "user.email", "test@test.com"]);
        run_git(&["config", "--local", "user.name", "Test User"]);
        run_git(&["config", "--local", "commit.gpgsign", "false"]);
        std::fs::write(dir.path().join("README.md"), "hello\n").expect("write readme");
        run_git(&["add", "README.md"]);
        run_git(&["commit", "-m", "init"]);
        run_git(&["branch", "-m", "main"]);
        // No dirty edits — NoDiff case.

        let _run_id = create_run_with_workspace(
            &db,
            &project.id,
            &task.id,
            Some(dir.path().to_str().unwrap()),
        )
        .await;

        let payload = Some(serde_json::json!({
            "task_id": task.id,
            "verdict": "rejected",
            "acceptance_criteria": [],
            "feedback": "needs more work"
        }));

        process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;

        let integrity_repo =
            djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository::new(db);
        let latest = integrity_repo.latest_for_task(&task.id).await.unwrap();
        assert!(
            latest.is_none(),
            "NoDiff worktree must not produce a rejected fingerprint record"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejected_review_skips_persistence_when_no_worktree() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        // Task run with no workspace_path (historical / no-worktree case).
        let _run_id = create_run_with_workspace(&db, &project.id, &task.id, None).await;

        let payload = Some(serde_json::json!({
            "task_id": task.id,
            "verdict": "rejected",
            "acceptance_criteria": [],
            "feedback": "no worktree available"
        }));

        process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;

        let integrity_repo =
            djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository::new(db);
        let latest = integrity_repo.latest_for_task(&task.id).await.unwrap();
        assert!(
            latest.is_none(),
            "no-worktree case must not produce a rejected fingerprint record"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn accepted_review_does_not_record_rejected_fingerprint() {
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        let worktree = init_git_repo_with_dirty_file();
        let _run_id = create_run_with_workspace(
            &db,
            &project.id,
            &task.id,
            Some(worktree.path().to_str().unwrap()),
        )
        .await;

        let payload = Some(serde_json::json!({
            "task_id": task.id,
            "verdict": "approved",
            "acceptance_criteria": [],
            "feedback": null
        }));

        process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;

        let integrity_repo =
            djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository::new(db);
        let latest = integrity_repo.latest_for_task(&task.id).await.unwrap();
        assert!(
            latest.is_none(),
            "approved review must not record rejected fingerprint"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejected_fingerprint_persists_across_task_run_boundaries() {
        // Simulate: task run 1 records a rejected fingerprint, then a new
        // task run 2 is created (redispatch). The latest_for_task query
        // must still see the rejection from run 1.
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        let worktree = init_git_repo_with_dirty_file();
        let run1_id = create_run_with_workspace(
            &db,
            &project.id,
            &task.id,
            Some(worktree.path().to_str().unwrap()),
        )
        .await;

        // Record the rejection via handle_submit_review.
        let payload = Some(serde_json::json!({
            "task_id": task.id,
            "verdict": "rejected",
            "acceptance_criteria": [],
            "feedback": "needs work"
        }));
        process_finalize_payload(&payload, "submit_review", &task.id, &ctx).await;

        let integrity_repo =
            djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository::new(
                db.clone(),
            );
        let latest = integrity_repo
            .latest_for_task(&task.id)
            .await
            .unwrap()
            .expect("rejection must be recorded");
        assert_eq!(latest.task_run_id.as_deref(), Some(run1_id.as_str()));
        assert_eq!(latest.no_progress_streak, 1);

        // Create a new task run (simulating redispatch).
        let _run2_id = create_run_with_workspace(
            &db,
            &project.id,
            &task.id,
            Some(worktree.path().to_str().unwrap()),
        )
        .await;

        // The latest rejection should still be from run 1 (cross-run persistence).
        let latest_after_redispatch = integrity_repo
            .latest_for_task(&task.id)
            .await
            .unwrap()
            .expect("must persist across task run boundaries");
        assert_eq!(
            latest_after_redispatch.task_run_id.as_deref(),
            Some(run1_id.as_str()),
            "rejection from run 1 must survive redispatch to run 2"
        );
        assert_eq!(
            latest_after_redispatch.diff_fingerprint,
            latest.diff_fingerprint
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn record_rejected_integrity_entry_direct_call_increments_streak() {
        // Test the shared helper directly: two consecutive rejections should
        // increment the streak from 0→1→2.
        let db = test_helpers::create_test_db();
        let ctx = test_helpers::agent_context_from_db(
            db.clone(),
            tokio_util::sync::CancellationToken::new(),
        );
        let project = test_helpers::create_test_project(&db).await;
        let epic = test_helpers::create_test_epic(&db, &project.id).await;
        let task = test_helpers::create_test_task(&db, &project.id, &epic.id).await;

        let run_id = create_run_with_workspace(&db, &project.id, &task.id, None).await;

        // First rejection.
        record_rejected_integrity_entry(
            &task.id,
            &ctx,
            djinn_core::models::RejectedVerdictKind::ReviewerReject.as_str(),
            None,
            Some(&run_id),
            "sha256:first-reject",
        )
        .await;

        let integrity_repo =
            djinn_db::repositories::verify_run::TaskRejectedSubmissionIntegrityRepository::new(
                db.clone(),
            );
        let latest = integrity_repo
            .latest_for_task(&task.id)
            .await
            .unwrap()
            .expect("first rejection must be recorded");
        assert_eq!(latest.no_progress_streak, 1);
        assert_eq!(latest.diff_fingerprint, "sha256:first-reject");

        // Second rejection (streak should be 2).
        record_rejected_integrity_entry(
            &task.id,
            &ctx,
            djinn_core::models::RejectedVerdictKind::ReviewerReject.as_str(),
            None,
            Some(&run_id),
            "sha256:second-reject",
        )
        .await;

        let latest2 = integrity_repo
            .latest_for_task(&task.id)
            .await
            .unwrap()
            .expect("second rejection must be recorded");
        assert_eq!(latest2.no_progress_streak, 2);
        assert_eq!(latest2.diff_fingerprint, "sha256:second-reject");
    }
}
