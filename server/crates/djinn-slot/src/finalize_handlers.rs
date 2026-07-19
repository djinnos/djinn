use crate::final_verification::validate_or_reverify_completion_intent;
use crate::finalize_types::{
    AcVerdict, AutoSubmitReviewMetadataPayload, SubmitDecision, SubmitGrooming, SubmitReview,
    SubmitWork,
};
use crate::host::SlotContext;
use crate::output_parser::CompletionIntent;
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
        "submit_work" => {
            let mut intent = CompletionIntent {
                finalize_payload: payload.clone(),
                tool_use_id: "finalize-payload".to_owned(),
                final_verification_evidence: None,
            };
            match validate_or_reverify_completion_intent(
                &mut intent,
                task_id,
                None,
                tokio_util::sync::CancellationToken::new(),
                app_state,
                "submit_work",
            )
            .await
            {
                // `Ok(None)` is the typed unconfigured-plan skip: the
                // submission proceeds with no evidence attached.
                Ok(evidence) => {
                    handle_submit_work(payload, task_id, app_state, true, evidence.as_ref()).await
                }
                Err(error) => {
                    tracing::warn!(task_id = %task_id, error = %error, "finalize_handlers: submit_work verification failed");
                    false
                }
            }
        }
        "submit_review" => {
            let mut intent = CompletionIntent {
                finalize_payload: payload.clone(),
                tool_use_id: "finalize-payload".to_owned(),
                final_verification_evidence: None,
            };
            match validate_or_reverify_completion_intent(
                &mut intent,
                task_id,
                None,
                tokio_util::sync::CancellationToken::new(),
                app_state,
                "submit_review",
            )
            .await
            {
                Ok(_) => {
                    handle_submit_review(payload, task_id, app_state).await;
                    true
                }
                Err(error) => {
                    tracing::warn!(task_id = %task_id, error = %error, "finalize_handlers: submit_review verification failed");
                    false
                }
            }
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

pub async fn process_completion_intent_with_outcome(
    intent: &CompletionIntent,
    finalize_tool_name: &str,
    task_id: &str,
    app_state: &SlotContext,
) -> bool {
    match finalize_tool_name {
        "submit_work" => {
            let mut intent = intent.clone();
            match validate_or_reverify_completion_intent(
                &mut intent,
                task_id,
                None,
                tokio_util::sync::CancellationToken::new(),
                app_state,
                "submit_work",
            )
            .await
            {
                // `Ok(None)` is the typed unconfigured-plan skip: the
                // submission proceeds with no evidence attached.
                Ok(evidence) => {
                    handle_submit_work(
                        &intent.finalize_payload,
                        task_id,
                        app_state,
                        true,
                        evidence.as_ref(),
                    )
                    .await
                }
                Err(error) => {
                    tracing::warn!(task_id = %task_id, error = %error, "finalize_handlers: completion C2 validation failed");
                    false
                }
            }
        }
        "submit_review" => {
            // A reviewer verdict mutates acceptance criteria and emits a
            // successful completion activity. Evidence captured at C1 cannot
            // be consumed after the worktree/environment may have changed.
            let mut intent = intent.clone();
            match validate_or_reverify_completion_intent(
                &mut intent,
                task_id,
                None,
                tokio_util::sync::CancellationToken::new(),
                app_state,
                "submit_review",
            )
            .await
            {
                Ok(_) => {
                    handle_submit_review(&intent.finalize_payload, task_id, app_state).await;
                    true
                }
                Err(error) => {
                    tracing::warn!(task_id = %task_id, error = %error, "finalize_handlers: reviewer completion C2 validation failed");
                    false
                }
            }
        }
        _ => {
            process_finalize_payload_with_outcome(
                &Some(intent.finalize_payload.clone()),
                finalize_tool_name,
                task_id,
                app_state,
            )
            .await
        }
    }
}

pub async fn process_auto_submit_payload(
    intent: &CompletionIntent,
    task_id: &str,
    app_state: &SlotContext,
) -> bool {
    let mut intent = intent.clone();
    match validate_or_reverify_completion_intent(
        &mut intent,
        task_id,
        None,
        tokio_util::sync::CancellationToken::new(),
        app_state,
        "submit_work",
    )
    .await
    {
        // `Ok(None)` is the typed unconfigured-plan skip: the auto-submit
        // proceeds with no evidence attached.
        Ok(evidence) => {
            handle_submit_work(
                &intent.finalize_payload,
                task_id,
                app_state,
                false,
                evidence.as_ref(),
            )
            .await
        }
        Err(error) => {
            tracing::warn!(task_id = %task_id, error = %error, "finalize_handlers: auto-submit C2 validation failed");
            false
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
    app_state: &SlotContext,
    model_called_submit_work: bool,
    final_verification_evidence: Option<
        &crate::final_verification::FinalVerificationSuccessEvidence,
    >,
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
        "final_verification_evidence": final_verification_evidence,
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
    let reason_label = reason.to_string();
    ctx.event_bus.send(DjinnEventEnvelope {
        entity_type: "verify",
        action: "submission_fingerprint_unavailable",
        payload: serde_json::json!({
            "task_id": task_id,
            "task_run_id": task_run_id,
            "reason": reason_label,
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
pub(crate) async fn handle_submit_review(
    payload: &serde_json::Value,
    task_id: &str,
    app_state: &SlotContext,
) {
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
        let fallback_run_id = runs.first().map(|r| r.id.as_str()).unwrap_or("unknown");
        emit_fingerprint_unavailable_event(
            task_id,
            fallback_run_id,
            "workspace_unavailable",
            app_state,
        );
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
            emit_fingerprint_unavailable_event(task_id, &task_run_id, "compute_error", app_state);
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
        emit_fingerprint_unavailable_event(task_id, &task_run_id, "no_diff", app_state);
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
