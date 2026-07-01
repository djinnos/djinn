//! Local gate pre-flight checks for the reviewer-approval path.
//!
//! Runs deterministic local gates before the supervisor transitions a
//! PR-backed task from `in_task_review` → `approved`.  Mirrors the
//! worker-submit gate check in [`super::pr_local_gates`] but with
//! review-specific failure routing:
//!
//! - **Failing required gate** → `Err(block_json)` with structured
//!   activity; caller rejects approval and routes to remediation.
//! - **Unavailable required gate** → `Err(block_json)` with structured
//!   activity; caller surfaces as blocking reviewer concern / lead
//!   intervention.
//! - **Passing / non-applicable / advisory** → `Ok(())`.

use std::path::Path;

use djinn_core::models::Task;
use djinn_db::TaskRepository;

use crate::local_gates::{
    self, CiGateInput, CommandRunner, GateOutcome, GatePlanResult, GateResult, ProcessRunner,
};

/// Event type emitted when a reviewer approval attempt is blocked by a
/// required local gate failure.
pub const REVIEW_LOCAL_GATE_BLOCK_EVENT: &str = "review_local_gate_block";

/// Verdict from the pure gate-evaluation logic (no DB side effects).
#[derive(Debug)]
pub(crate) enum ReviewGateVerdict {
    /// All required gates passed or no gates applied — approval may proceed.
    Allow,
    /// A required gate blocked approval.  Carries the structured block JSON
    /// and the `GatePlanResult` so the caller can record activity.
    Block {
        block_json: String,
        result: GatePlanResult,
    },
}

/// Pure gate-evaluation logic: build a plan from the task's CI metadata,
/// execute it with the given runner, and return a verdict.
///
/// This function has no DB side effects — it is the testable core.
pub(crate) async fn evaluate_review_gates(
    task: &Task,
    workspace_root: &Path,
    runner: &dyn CommandRunner,
) -> ReviewGateVerdict {
    let ci_input = CiGateInput::from_task_fields(
        &task.ci_blocking_required_check_names,
        task.ci_failure_fingerprint.clone(),
        task.ci_last_remediation_base_sha.clone(),
        Vec::new(),
    );
    let plan = local_gates::build_plan(&ci_input);

    if !plan.has_required() {
        return ReviewGateVerdict::Allow;
    }

    let result = local_gates::execute_plan(&plan, workspace_root, runner).await;

    if result.has_blocking_failure() {
        let gate_ids: Vec<&str> = result.blocking_gate_ids();
        let has_unavailable = result
            .results
            .iter()
            .any(|r| r.is_blocking_failure() && r.outcome == GateOutcome::Unavailable);

        let block_json = serde_json::json!({
            "gate_ids": gate_ids,
            "has_unavailable": has_unavailable,
            "blocking_reason": if has_unavailable {
                format!(
                    "required local gates unavailable: {}",
                    gate_ids.join(", ")
                )
            } else {
                format!(
                    "required local gates failed: {}",
                    gate_ids.join(", ")
                )
            },
            "details": result
                .results
                .iter()
                .filter(|r| r.is_blocking_failure())
                .map(gate_result_to_detail_json)
                .collect::<Vec<_>>(),
        });

        ReviewGateVerdict::Block {
            block_json: block_json.to_string(),
            result,
        }
    } else {
        ReviewGateVerdict::Allow
    }
}

/// Run required deterministic local gates for a PR-backed task before
/// reviewer approval.
///
/// Returns `Ok(())` when all required gates pass (or no gates apply),
/// allowing the caller to proceed with `task_review_approve`.  Returns
/// `Err(block_json)` when a required gate fails or is unavailable — the
/// caller must prevent approval and route the task appropriately.
///
/// `workspace_root` is the path to the ephemeral clone already checked out
/// on the task branch (the supervisor's `workspace.path()`).
pub async fn check_local_gates_for_review(
    task: &Task,
    task_repo: &TaskRepository,
    workspace_root: &Path,
) -> Result<(), String> {
    let runner = ProcessRunner;
    match evaluate_review_gates(task, workspace_root, &runner).await {
        ReviewGateVerdict::Allow => Ok(()),
        ReviewGateVerdict::Block { block_json, result } => {
            record_review_gate_block(task, task_repo, &result).await;
            Err(block_json)
        }
    }
}

/// Record a structured activity event for a reviewer local gate block.
async fn record_review_gate_block(
    task: &Task,
    task_repo: &TaskRepository,
    result: &GatePlanResult,
) {
    let gate_ids: Vec<&str> = result.blocking_gate_ids();
    let details: Vec<serde_json::Value> = result
        .results
        .iter()
        .filter(|r| r.is_blocking_failure())
        .map(gate_result_to_detail_json)
        .collect();

    let has_unavailable = result
        .results
        .iter()
        .any(|r| r.is_blocking_failure() && r.outcome == GateOutcome::Unavailable);

    // Structured activity event.
    let payload = serde_json::json!({
        "task_id": task.id,
        "short_id": task.short_id,
        "gate_ids": gate_ids,
        "has_unavailable": has_unavailable,
        "details": details,
        "blocking_reason": if has_unavailable {
            format!(
                "required local gates unavailable: {}",
                gate_ids.join(", ")
            )
        } else {
            format!(
                "required local gates failed: {}",
                gate_ids.join(", ")
            )
        },
    });
    if let Err(e) = task_repo
        .log_activity(
            Some(&task.id),
            "coordinator",
            "system",
            REVIEW_LOCAL_GATE_BLOCK_EVENT,
            &payload.to_string(),
        )
        .await
    {
        tracing::warn!(
            task_id = %task.short_id,
            error = %e,
            "reviewer local gate: failed to emit block activity event"
        );
    }

    // Human-readable comment.
    let comment = format_review_gate_block_comment(&gate_ids, result, has_unavailable);
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
            "reviewer local gate: failed to emit block comment"
        );
    }

    tracing::warn!(
        task_id = %task.short_id,
        gate_ids = ?gate_ids,
        has_unavailable,
        "reviewer local gate: required gates blocked approval"
    );
}

/// Convert a blocking `GateResult` into a JSON detail object.
fn gate_result_to_detail_json(r: &GateResult) -> serde_json::Value {
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

/// Format a human-readable comment summarising the reviewer local gate block.
fn format_review_gate_block_comment(
    gate_ids: &[&str],
    result: &GatePlanResult,
    has_unavailable: bool,
) -> String {
    let header = if has_unavailable {
        "**⚠ Local gate block (unavailable)** — required deterministic \
         pre-flight checks could not run (command or working directory \
         unavailable).  Reviewer approval is blocked until the gate \
         commands are available or the blocking concern is resolved."
            .to_string()
    } else {
        "**⚠ Local gate block** — required deterministic pre-flight \
         checks failed.  Reviewer approval is blocked; the task needs \
         remediation before it can be approved."
            .to_string()
    };

    let mut lines = vec![header];

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
fn truncate_for_comment(s: &str, max_len: usize) -> String {
    let flat = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.len() <= max_len {
        flat
    } else {
        format!("{}…", &flat[..max_len])
    }
}

#[cfg(test)]
mod tests;
