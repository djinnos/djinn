//! BLOCKING red-CI directive rendering for the worker prompt.
//!
//! Renders the promoted CI directive from the task's durable CI snapshot
//! fields: the PR-head required-CI failure section and the merge-queue
//! (`merge_group`) rejection section.

use djinn_core::models::Task;

/// Build BLOCKING directive for red required CI. Returns None for passing/advisory states.
///
/// Combines two independent sections: the PR-head required-CI failure section
/// and the merge-queue (`merge_group`) rejection section. A PR whose own head
/// checks are green can still be dequeued by the merge queue, so the
/// merge-queue section renders even when the PR-head lane is not failing.
pub(crate) fn build_ci_blocking_directive(task: &Task) -> Option<String> {
    let head = build_ci_head_blocking_directive(task);
    let merge_queue = build_ci_merge_queue_directive(task);
    match (head, merge_queue) {
        (None, None) => None,
        (Some(h), None) => Some(h),
        (None, Some(m)) => Some(m),
        (Some(h), Some(m)) => Some(format!("{h}\n\n{m}")),
    }
}

/// PR-head required-CI failure section of the blocking directive.
fn build_ci_head_blocking_directive(task: &Task) -> Option<String> {
    if task.ci_status != "failing" {
        return None;
    }
    let base_sha = task.ci_last_remediation_base_sha.as_deref()?;
    let head_sha = task.ci_head_sha.as_deref().unwrap_or("unknown");
    let pr_number = task.ci_pr_number.unwrap_or(0);
    let check_names: Vec<String> =
        serde_json::from_str(&task.ci_blocking_required_check_names).unwrap_or_default();
    let checks_display = if check_names.is_empty() {
        "unknown".to_string()
    } else {
        check_names.join(", ")
    };
    let fingerprint_line = match &task.ci_failure_fingerprint {
        Some(fp) => format!("**Failure fingerprint:** `{fp}`\n"),
        None => String::new(),
    };
    Some(format!(
        "**PR:** #{pr_number}\\\n\
         **Failing head SHA:** `{head_sha}`\\\n\
         **Blocking checks:** {checks_display}\\\n\
         {fingerprint_line}\
         **Remediation baseline SHA:** `{base_sha}`\n\n\
         > REQUIRED CI is failing on the current PR head. You MUST fix the \
         failing required checks listed above before this task can proceed. \
         The task will remain in remediation until all blocking checks pass \
         on a new commit pushed to the PR branch."
    ))
}

/// Merge-queue (`merge_group`) rejection section of the blocking directive.
///
/// Renders only when the durable CI snapshot carries a merge-queue failure
/// lane (`ci_mq_state` present). These heavy `merge_group` checks run on the
/// ephemeral queue ref, not the PR head, so a green PR head does not clear
/// them.
fn build_ci_merge_queue_directive(task: &Task) -> Option<String> {
    let state = task.ci_mq_state.as_deref()?;
    let run_display = match task.ci_mq_run_id {
        Some(id) => format!("`{id}`"),
        None => "unknown".to_string(),
    };
    let check_names: Vec<String> = task
        .ci_mq_failed_check_names
        .as_deref()
        .and_then(|raw| serde_json::from_str(raw).ok())
        .unwrap_or_default();
    let checks_display = if check_names.is_empty() {
        "unknown".to_string()
    } else {
        check_names.join(", ")
    };
    let count = task.ci_mq_same_signature_count.unwrap_or(0);
    let suffix = ordinal_suffix(count);
    let fingerprint_line = match &task.ci_mq_failure_fingerprint {
        Some(fp) => format!("**Merge-queue failure fingerprint:** `{fp}`\\\n"),
        None => String::new(),
    };
    Some(format!(
        "**Merge-queue state:** {state}\\\n\
         **Merge-group run:** {run_display}\\\n\
         **Merge-group failing checks:** {checks_display}\\\n\
         {fingerprint_line}\
         **Queue rejection streak:** {count}{suffix} consecutive same-signature \
         queue rejection\n\n\
         > This PR was accepted onto GitHub's merge queue but the `merge_group` \
         CI stages failed, so it was dequeued. These heavy checks run only on \
         the ephemeral queue ref, not the PR head, so a green PR head does NOT \
         clear them. You MUST fix the merge-group failing checks listed above \
         before this task can merge."
    ))
}

/// English ordinal suffix (`st`/`nd`/`rd`/`th`) for a 1-based count.
fn ordinal_suffix(n: i64) -> &'static str {
    let n = n.abs();
    if (11..=13).contains(&(n % 100)) {
        return "th";
    }
    match n % 10 {
        1 => "st",
        2 => "nd",
        3 => "rd",
        _ => "th",
    }
}
