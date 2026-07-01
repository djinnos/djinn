//! Local gate pre-flight checks for the PR-open path.
//!
//! Extracted from [`super::pr`] to keep the supervisor PR-open module under
//! the server-size-guard limit.  All functions here are called only from
//! `pr.rs` and its companion test file.

use djinn_core::models::TransitionAction;
use djinn_db::{ActivityQuery, TaskRepository};
use djinn_workspace::MirrorManager;

use crate::local_gates::{
    self, CiGateInput, GateOutcome, GatePlanResult, GateResult, ProcessRunner,
};

use djinn_runtime::spec::TaskRunOutcome;

/// Event type emitted when a submit/PR-open attempt is blocked by a required
/// local gate failure (deterministic pre-flight check). Consumed by the
/// activity log as a blocking system event.
pub(crate) const LOCAL_GATE_BLOCK_EVENT: &str = "local_gate_block";

/// Run required deterministic local gates from the task branch workspace
/// before pushing to GitHub.
///
/// Returns `Some(TaskRunOutcome::Escalated { .. })` when a required gate
/// fails or is unavailable — the caller must NOT proceed to the GitHub
/// push/PR-open path.  Returns `None` when all required gates pass (or no
/// gates applied) — the caller proceeds normally.
///
/// The check is **SHA-aware**: if a `local_gate_block` activity was already
/// recorded for the same commit SHA, the gates are skipped (the block is
/// durable until a new commit changes the code).  This prevents the
/// approved→open→approved→… re-dispatch loop from re-running gates on every
/// tick.
pub(super) async fn check_local_gates_before_pr(
    task: &djinn_core::models::Task,
    mirror: &MirrorManager,
    task_repo: &TaskRepository,
    task_branch: &str,
) -> Option<TaskRunOutcome> {
    // Build a plan from the task's structured CI metadata.
    let ci_input = CiGateInput::from_task_fields(
        &task.ci_blocking_required_check_names,
        task.ci_failure_fingerprint.clone(),
        task.ci_last_remediation_base_sha.clone(),
        Vec::new(),
    );
    let plan = local_gates::build_plan(&ci_input);

    if !plan.has_required() {
        return None;
    }

    // Clone the workspace on the task branch to get the HEAD SHA and run gates.
    let workspace = match mirror.clone_ephemeral(&task.project_id, task_branch).await {
        Ok(ws) => ws,
        Err(e) => {
            // If the branch doesn't exist in the mirror, we can't run gates.
            // Fall through to the normal push path (which will surface the
            // real error).
            tracing::debug!(
                task_id = %task.short_id,
                task_branch,
                error = %e,
                "supervisor PR-open: local gate check skipped — could not clone task branch from mirror"
            );
            return None;
        }
    };

    // Resolve the current HEAD SHA of the task branch.
    let head_sha = match djinn_git::run_git_command(
        workspace.path_buf(),
        vec!["rev-parse".into(), "HEAD".into()],
    )
    .await
    {
        Ok(out) => out.stdout.trim().to_string(),
        Err(e) => {
            tracing::debug!(
                task_id = %task.short_id,
                error = %e,
                "supervisor PR-open: local gate check skipped — could not resolve HEAD SHA"
            );
            return None;
        }
    };

    // SHA-aware dedup: if a `local_gate_block` activity was already recorded
    // for this task with the same gate IDs and commit SHA, skip re-running
    // gates (the block is durable until a new commit changes the code).
    let plan_gate_ids = plan.ids();
    if has_matching_local_gate_block(task_repo, &task.id, &plan_gate_ids, &head_sha).await {
        tracing::debug!(
            task_id = %task.short_id,
            head_sha = %head_sha,
            gate_ids = ?plan_gate_ids,
            "supervisor PR-open: local gate block already recorded for this commit — skipping"
        );
        // Return Some(Escalated) to block the PR-open.  The task will be
        // transitioned to `open` for remediation (the caller handles this).
        return Some(TaskRunOutcome::Escalated {
            reason: format!(
                "Local gate block already recorded for commit {head_sha}; \
                 a new commit is required to clear the block."
            ),
        });
    }

    // Execute the gate plan.
    let runner = ProcessRunner;
    let result = local_gates::execute_plan(&plan, workspace.path(), &runner).await;

    if result.has_blocking_failure() {
        // Record structured activity and park the task for remediation.
        Some(record_local_gate_block(task, task_repo, &head_sha, &result).await)
    } else {
        None
    }
}

/// Check the task's activity log for an existing `local_gate_block` event
/// whose gate IDs and commit SHA match the current plan.  Returns `true`
/// when a matching block exists — the caller should skip re-running gates.
async fn has_matching_local_gate_block(
    task_repo: &TaskRepository,
    task_id: &str,
    plan_gate_ids: &[&str],
    current_sha: &str,
) -> bool {
    let activity = match task_repo
        .query_activity(ActivityQuery {
            task_id: Some(task_id.to_string()),
            limit: 5,
            ..Default::default()
        })
        .await
    {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(
                task_id,
                error = %e,
                "supervisor PR-open: could not query activity for local gate dedup"
            );
            return false;
        }
    };

    let block = activity
        .iter()
        .find(|e| e.event_type == LOCAL_GATE_BLOCK_EVENT);
    let block = match block {
        Some(b) => b,
        None => return false,
    };

    let payload: serde_json::Value = match serde_json::from_str(&block.payload) {
        Ok(v) => v,
        Err(_) => return false,
    };

    // Check SHA match.
    let recorded_sha = match payload.get("head_sha").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return false,
    };
    if recorded_sha != current_sha {
        return false;
    }

    // Check gate IDs match.
    let recorded_ids: Vec<String> = payload
        .get("gate_ids")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let mut sorted_plan: Vec<&str> = plan_gate_ids.to_vec();
    sorted_plan.sort();
    let mut sorted_recorded = recorded_ids.clone();
    sorted_recorded.sort();
    sorted_plan == sorted_recorded.as_slice()
}

/// Record a local gate block: emit structured activity, a human-readable
/// comment, and park the task for remediation.  Returns
/// `TaskRunOutcome::Escalated`.
async fn record_local_gate_block(
    task: &djinn_core::models::Task,
    task_repo: &TaskRepository,
    head_sha: &str,
    result: &GatePlanResult,
) -> TaskRunOutcome {
    let gate_ids: Vec<&str> = result.blocking_gate_ids();
    let details: Vec<serde_json::Value> = result
        .results
        .iter()
        .filter(|r| r.is_blocking_failure())
        .map(gate_result_to_detail_json)
        .collect();

    // Structured activity event.
    let payload = serde_json::json!({
        "task_id": task.id,
        "short_id": task.short_id,
        "head_sha": head_sha,
        "gate_ids": gate_ids,
        "details": details,
        "blocking_reason": format!(
            "required local gates failed: {}",
            gate_ids.join(", ")
        ),
    });
    if let Err(e) = task_repo
        .log_activity(
            Some(&task.id),
            "coordinator",
            "system",
            LOCAL_GATE_BLOCK_EVENT,
            &payload.to_string(),
        )
        .await
    {
        tracing::warn!(
            task_id = %task.short_id,
            error = %e,
            "supervisor PR-open: failed to emit local gate block activity event"
        );
    }

    // Human-readable comment.
    let comment = format_local_gate_block_comment(head_sha, &gate_ids, result);
    let comment_payload = serde_json::json!({ "body": comment });
    if let Err(e) = task_repo
        .log_activity(
            Some(&task.id),
            "coordinator",
            "system",
            "comment",
            &comment_payload.to_string(),
        )
        .await
    {
        tracing::warn!(
            task_id = %task.short_id,
            error = %e,
            "supervisor PR-open: failed to emit local gate block comment"
        );
    }

    // Park the task for remediation — move it back to `open` so the worker
    // can fix the gate failures.  `ParkForRemediation` is legal from all
    // pre-terminal in-flight states (including `approved` and `in_progress`)
    // and does NOT bump `reopen_count`.
    let reason = format!(
        "required local gates failed ({}); parked for remediation",
        gate_ids.join(", ")
    );
    if let Err(e) = task_repo
        .transition(
            &task.id,
            TransitionAction::ParkForRemediation,
            "coordinator",
            "system",
            Some(&reason),
            None,
        )
        .await
    {
        tracing::warn!(
            task_id = %task.short_id,
            status = %task.status,
            error = %e,
            "supervisor PR-open: local gate park_for_remediation transition skipped \
             (task may not be in a parkable state — it stays where it is)"
        );
    }

    tracing::warn!(
        task_id = %task.short_id,
        head_sha = %head_sha,
        gate_ids = ?gate_ids,
        "supervisor PR-open: required local gates failed — task parked for remediation, no PR opened"
    );

    TaskRunOutcome::Escalated {
        reason: format!(
            "required local gates failed ({}); task parked for remediation",
            gate_ids.join(", ")
        ),
    }
}

/// Convert a blocking `GateResult` into a JSON detail object for the
/// structured activity event.
pub(crate) fn gate_result_to_detail_json(r: &GateResult) -> serde_json::Value {
    serde_json::json!({
        "gate_id": r.gate_id,
        "outcome": format!("{:?}", r.outcome).to_lowercase(),
        "command": r.command,
        "cwd": r.cwd,
        "timeout_secs": r.timeout.as_secs(),
        "exit_code": r.exit_code,
        "stdout_summary": r.stdout_summary,
        "stderr_summary": r.stderr_summary,
        "duration_ms": r.duration.map(|d| d.as_millis() as u64),
        "artifact": r.artifact,
        "blocking_reason": match r.outcome {
            GateOutcome::Unavailable => "required command or working directory unavailable",
            _ => "required gate command exited non-zero",
        },
    })
}

/// Format a human-readable comment summarising the local gate block.
pub(crate) fn format_local_gate_block_comment(
    head_sha: &str,
    gate_ids: &[&str],
    result: &GatePlanResult,
) -> String {
    let mut lines = vec![format!(
        "**⚠ Local gate block** — required deterministic pre-flight checks failed \
         (commit `{head_sha}`).  The task is held for remediation; a new commit \
         fixing the gate failures is required before the PR can be opened."
    )];

    for r in &result.results {
        if !r.is_blocking_failure() {
            continue;
        }
        let status = match r.outcome {
            GateOutcome::Unavailable => "unavailable".to_string(),
            GateOutcome::Failed => match r.exit_code {
                Some(code) => format!("failed (exit {code})"),
                None => "failed".to_string(),
            },
            _ => format!("{:?}", r.outcome).to_lowercase(),
        };
        lines.push(format!(
            "- **{}** — `{}` in `{}` (timeout {}s) — {}",
            r.gate_id,
            r.command.join(" "),
            r.cwd,
            r.timeout.as_secs(),
            status,
        ));

        if !r.stderr_summary.is_empty() {
            lines.push(format!(
                "  stderr: {}",
                truncate_for_comment(&r.stderr_summary, 200)
            ));
        }
        if !r.stdout_summary.is_empty() {
            lines.push(format!(
                "  stdout: {}",
                truncate_for_comment(&r.stdout_summary, 200)
            ));
        }
    }

    lines.push(format!("Blocked gates: {}", gate_ids.join(", ")));

    lines.join("\n")
}

/// Truncate a string for inline use in a comment, preserving head.
pub(crate) fn truncate_for_comment(s: &str, max_len: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.len() <= max_len {
        flat
    } else {
        format!("{}…", &flat[..max_len])
    }
}
