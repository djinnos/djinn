use super::*;
use djinn_core::models::Task;

fn task_with_merge_commit_sha(merge_commit_sha: Option<&str>) -> Task {
    Task {
        id: "task-1".into(),
        project_id: "project-1".into(),
        short_id: "T-1".into(),
        epic_id: Some("epic-1".into()),
        title: "Landed work".into(),
        description: "".into(),
        design: "".into(),
        issue_type: "task".into(),
        status: "closed".into(),
        priority: 0,
        owner: "".into(),
        labels: "[]".into(),
        acceptance_criteria: "[]".into(),
        reopen_count: 0,
        continuation_count: 0,
        total_reopen_count: 0,
        intervention_count: 0,
        last_intervention_at: None,
        created_at: "2026-06-14T00:00:00Z".into(),
        updated_at: "2026-06-14T00:01:00Z".into(),
        closed_at: Some("2026-06-14T00:02:00Z".into()),
        close_reason: Some("completed".into()),
        merge_commit_sha: merge_commit_sha.map(str::to_owned),
        pr_url: None,
        merge_conflict_metadata: None,
        memory_refs: "[]".into(),
        agent_type: None,
        created_by_user_id: None,
        ci_status: "unknown".into(),
        ci_head_sha: None,
        ci_pr_number: None,
        ci_blocking_required_check_names: "[]".into(),
        ci_failure_fingerprint: None,
        ci_first_seen_at: None,
        ci_last_seen_at: None,
        ci_same_signature_count: 0,
        ci_last_remediation_base_sha: None,
        ci_mirror_head_sha: None,
        ci_github_head_sha: None,
        ci_heads_diverged: None,
        ci_head_observation_error: None,
        unresolved_blocker_count: 0,
    }
}

#[test]
fn task_list_item_serialization_preserves_merge_commit_sha() {
    let sha = "abc123def4567890abc123def4567890abc123de";
    let task = task_with_merge_commit_sha(Some(sha));

    let list_item = task_to_list_item(&task, None, 0);
    let serialized = serde_json::to_value(&list_item).unwrap();

    assert_eq!(list_item.merge_commit_sha.as_deref(), Some(sha));
    assert_eq!(serialized["merge_commit_sha"], sha);
}

fn task_with_ci_snapshot() -> Task {
    let mut task = task_with_merge_commit_sha(None);
    task.ci_status = "failing".into();
    task.ci_head_sha = Some("deadbeefcafebabe00000000000000000000ffff".into());
    task.ci_pr_number = Some(42);
    task.ci_blocking_required_check_names = r#"["Server Size Guard","clippy"]"#.into();
    task.ci_failure_fingerprint = Some("sha:deadbeef|checks:clippy,size".into());
    task.ci_first_seen_at = Some("2026-06-14T00:00:00Z".into());
    task.ci_last_seen_at = Some("2026-06-14T00:05:00Z".into());
    task.ci_same_signature_count = 3;
    task.ci_last_remediation_base_sha = Some("base1234567890".into());
    task
}

#[test]
fn task_response_exposes_ci_gate_snapshot_when_present() {
    let task = task_with_ci_snapshot();
    let response = task_to_response(&task);
    let serialized = serde_json::to_value(&response).unwrap();

    let ci = serialized["ci"]
        .as_object()
        .expect("ci should be an object");
    assert_eq!(ci["status"], "failing");
    assert_eq!(ci["head_sha"], "deadbeefcafebabe00000000000000000000ffff");
    assert_eq!(ci["blocking_required_check_names"][0], "Server Size Guard");
    assert_eq!(ci["blocking_required_check_names"][1], "clippy");
    assert_eq!(ci["failure_fingerprint"], "sha:deadbeef|checks:clippy,size");
    assert_eq!(ci["first_seen_at"], "2026-06-14T00:00:00Z");
    assert_eq!(ci["last_seen_at"], "2026-06-14T00:05:00Z");
    assert_eq!(ci["same_signature_count"], 3);
    assert_eq!(ci["last_remediation_base_sha"], "base1234567890");
    assert_eq!(ci["pr_number"], 42);

    // Derived fields from upstream CI gate model
    assert_eq!(ci["gate_state"], "failing");
    assert_eq!(ci["primary_blocking_check"], "Server Size Guard");
    assert!(
        ci["summary_reason"]
            .as_str()
            .unwrap()
            .contains("Server Size Guard"),
        "summary_reason names the primary blocking check"
    );
    assert!(
        ci["merge_blocked_reason"].is_string(),
        "non-passing has merge_blocked_reason"
    );
    // Top-level aliases expose the requested human/agent field names while
    // still sourcing values from the durable structured snapshot.
    assert_eq!(serialized["ci_status"], "failing");
    assert_eq!(serialized["ci_gate_state"], "failing");
    assert_eq!(serialized["ci_primary_blocking_check"], "Server Size Guard");
    assert_eq!(serialized["ci_summary_reason"], ci["summary_reason"]);
    assert_eq!(
        serialized["ci_merge_blocked_reason"],
        ci["merge_blocked_reason"]
    );
}

#[test]
fn task_response_omits_ci_when_snapshot_absent() {
    // Default task has ci_head_sha = None → no snapshot.
    let task = task_with_merge_commit_sha(None);
    let response = task_to_response(&task);
    let serialized = serde_json::to_value(&response).unwrap();

    assert!(serialized.get("ci").is_none() || serialized["ci"].is_null());
    assert_eq!(serialized["ci_status"], "unknown");
    assert_eq!(serialized["ci_gate_state"], "unknown");
    assert!(response.ci.is_none());
}

#[test]
fn task_list_item_exposes_ci_gate_snapshot_when_present() {
    let task = task_with_ci_snapshot();
    let list_item = task_to_list_item(&task, None, 0);
    let serialized = serde_json::to_value(&list_item).unwrap();

    assert_eq!(serialized["ci"]["status"], "failing");
    assert_eq!(
        serialized["ci"]["head_sha"],
        "deadbeefcafebabe00000000000000000000ffff"
    );
    // Derived fields also present in list items
    assert_eq!(serialized["ci"]["gate_state"], "failing");
    assert_eq!(
        serialized["ci"]["primary_blocking_check"],
        "Server Size Guard"
    );
    assert_eq!(serialized["ci_status"], "failing");
    assert_eq!(serialized["ci_gate_state"], "failing");
    assert_eq!(serialized["ci_primary_blocking_check"], "Server Size Guard");
}

#[test]
fn ci_status_enum_serializes_to_snake_case_wire_values() {
    assert_eq!(serde_json::to_value(CiStatus::Passing).unwrap(), "passing");
    assert_eq!(serde_json::to_value(CiStatus::Failing).unwrap(), "failing");
    assert_eq!(serde_json::to_value(CiStatus::Pending).unwrap(), "pending");
    assert_eq!(serde_json::to_value(CiStatus::Unknown).unwrap(), "unknown");
}

// ── ci_status exact wire values in task DTOs ─────────────────────────

#[test]
fn task_dto_ci_status_serializes_exact_wire_values() {
    for (ci_status_str, expected) in [
        ("passing", "passing"),
        ("failing", "failing"),
        ("pending", "pending"),
        ("unknown", "unknown"),
    ] {
        let mut task = task_with_merge_commit_sha(None);
        task.ci_status = ci_status_str.into();
        task.ci_head_sha = Some("abcdef".into());
        let response = task_to_response(&task);
        let serialized = serde_json::to_value(&response).unwrap();
        assert_eq!(
            serialized["ci"]["status"], expected,
            "ci_status={ci_status_str} should serialize as {expected}"
        );
        assert_eq!(
            serialized["ci_status"], expected,
            "top-level ci_status={ci_status_str} should serialize as {expected}"
        );
    }
}

// ── gate_state: awaiting_ci for pr_draft + pending/unknown ───────────

fn task_with_ci_status_and_status(ci_status: &str, task_status: &str) -> Task {
    let mut task = task_with_merge_commit_sha(None);
    task.ci_status = ci_status.into();
    task.status = task_status.into();
    task.ci_head_sha = Some("abcdef".into());
    task
}

#[test]
fn gate_state_awaiting_ci_for_pr_draft_pending() {
    let task = task_with_ci_status_and_status("pending", "pr_draft");
    let response = task_to_response(&task);
    let serialized = serde_json::to_value(&response).unwrap();
    assert_eq!(serialized["ci"]["gate_state"], "awaiting_ci");
    assert_eq!(serialized["ci_gate_state"], "awaiting_ci");
}

#[test]
fn gate_state_awaiting_ci_for_pr_draft_unknown() {
    let task = task_with_ci_status_and_status("unknown", "pr_draft");
    let response = task_to_response(&task);
    let serialized = serde_json::to_value(&response).unwrap();
    assert_eq!(serialized["ci"]["gate_state"], "awaiting_ci");
    assert_eq!(serialized["ci_gate_state"], "awaiting_ci");
}

#[test]
fn gate_state_passing_for_pr_draft_when_passing() {
    let task = task_with_ci_status_and_status("passing", "pr_draft");
    let response = task_to_response(&task);
    let serialized = serde_json::to_value(&response).unwrap();
    assert_eq!(serialized["ci"]["gate_state"], "passing");
}

#[test]
fn gate_state_failing_for_pr_draft_when_failing() {
    let task = task_with_ci_status_and_status("failing", "pr_draft");
    let response = task_to_response(&task);
    let serialized = serde_json::to_value(&response).unwrap();
    assert_eq!(serialized["ci"]["gate_state"], "failing");
}

#[test]
fn gate_state_pending_for_non_pr_draft_when_pending() {
    let task = task_with_ci_status_and_status("pending", "in_progress");
    let response = task_to_response(&task);
    let serialized = serde_json::to_value(&response).unwrap();
    assert_eq!(serialized["ci"]["gate_state"], "pending");
}

#[test]
fn gate_state_unknown_for_non_pr_draft_when_unknown() {
    let task = task_with_ci_status_and_status("unknown", "open");
    let response = task_to_response(&task);
    let serialized = serde_json::to_value(&response).unwrap();
    assert_eq!(serialized["ci"]["gate_state"], "unknown");
}

// ── CiGateState serialization ────────────────────────────────────────

#[test]
fn ci_gate_state_serializes_to_exact_wire_values() {
    assert_eq!(
        serde_json::to_value(CiGateState::Passing).unwrap(),
        "passing"
    );
    assert_eq!(
        serde_json::to_value(CiGateState::Failing).unwrap(),
        "failing"
    );
    assert_eq!(
        serde_json::to_value(CiGateState::Pending).unwrap(),
        "pending"
    );
    assert_eq!(
        serde_json::to_value(CiGateState::Unknown).unwrap(),
        "unknown"
    );
    assert_eq!(
        serde_json::to_value(CiGateState::AwaitingCi).unwrap(),
        "awaiting_ci"
    );
}

// ── failing: primary blocking check and summary reason ───────────────

#[test]
fn failing_ci_serializes_primary_blocking_check() {
    let mut task = task_with_merge_commit_sha(None);
    task.ci_status = "failing".into();
    task.ci_head_sha = Some("abcdef".into());
    task.ci_blocking_required_check_names = r#"["Quality Gate","clippy"]"#.into();
    let response = task_to_response(&task);
    let serialized = serde_json::to_value(&response).unwrap();
    let ci = &serialized["ci"];

    // Alphabetically sorted: Quality Gate < clippy
    assert_eq!(ci["primary_blocking_check"], "Quality Gate");
    assert!(
        ci["summary_reason"]
            .as_str()
            .unwrap()
            .contains("Quality Gate"),
        "summary_reason should name the primary blocking check"
    );
    assert!(
        ci["merge_blocked_reason"]
            .as_str()
            .unwrap()
            .contains("Quality Gate"),
        "merge_blocked_reason should name the primary blocking check"
    );
}

#[test]
fn failing_ci_with_empty_checks_has_no_primary_blocking_check() {
    let mut task = task_with_merge_commit_sha(None);
    task.ci_status = "failing".into();
    task.ci_head_sha = Some("abcdef".into());
    task.ci_blocking_required_check_names = "[]".into();
    let response = task_to_response(&task);
    let serialized = serde_json::to_value(&response).unwrap();
    let ci = &serialized["ci"];

    assert!(
        ci.get("primary_blocking_check").is_none() || ci["primary_blocking_check"].is_null(),
        "no blocking checks means no primary_blocking_check"
    );
    assert_eq!(ci["summary_reason"], "Required checks failing");
}

// ── pending/unknown: visible non-terminal states, not failures ──────

#[test]
fn pending_ci_is_visible_non_terminal_not_failure() {
    let mut task = task_with_merge_commit_sha(None);
    task.ci_status = "pending".into();
    task.ci_head_sha = Some("abcdef".into());
    let response = task_to_response(&task);
    let serialized = serde_json::to_value(&response).unwrap();
    let ci = &serialized["ci"];

    assert_eq!(ci["status"], "pending");
    assert_eq!(ci["gate_state"], "pending");
    assert_eq!(ci["summary_reason"], "Required checks pending");
    assert_eq!(
        ci["merge_blocked_reason"],
        "Waiting for required checks to complete"
    );
    // No failure signals
    assert!(ci.get("primary_blocking_check").is_none() || ci["primary_blocking_check"].is_null());
    assert!(ci.get("failure_fingerprint").is_none() || ci["failure_fingerprint"].is_null());
}

#[test]
fn unknown_ci_is_visible_non_terminal_not_failure() {
    let mut task = task_with_merge_commit_sha(None);
    task.ci_status = "unknown".into();
    task.ci_head_sha = Some("abcdef".into());
    let response = task_to_response(&task);
    let serialized = serde_json::to_value(&response).unwrap();
    let ci = &serialized["ci"];

    assert_eq!(ci["status"], "unknown");
    assert_eq!(ci["gate_state"], "unknown");
    assert_eq!(ci["summary_reason"], "CI state unknown");
    assert!(
        ci["merge_blocked_reason"]
            .as_str()
            .unwrap()
            .contains("cannot confirm"),
        "unknown merge_blocked_reason explains uncertainty"
    );
    // No failure signals
    assert!(ci.get("primary_blocking_check").is_none() || ci["primary_blocking_check"].is_null());
}

// ── passing: no merge_blocked_reason ─────────────────────────────────

#[test]
fn passing_ci_has_no_merge_blocked_reason() {
    let mut task = task_with_merge_commit_sha(None);
    task.ci_status = "passing".into();
    task.ci_head_sha = Some("abcdef".into());
    let response = task_to_response(&task);
    let serialized = serde_json::to_value(&response).unwrap();
    let ci = &serialized["ci"];

    assert_eq!(ci["status"], "passing");
    assert_eq!(ci["gate_state"], "passing");
    assert_eq!(ci["summary_reason"], "All required checks passed");
    assert!(
        ci.get("merge_blocked_reason").is_none() || ci["merge_blocked_reason"].is_null(),
        "passing CI should have no merge_blocked_reason"
    );
}

#[test]
fn regression_required_red_ci_blocks_closed_presentation_from_structured_snapshot() {
    let mut task = task_with_merge_commit_sha(None);
    task.status = "closed".into();
    task.ci_status = "failing".into();
    task.ci_head_sha = Some("failing1234567890".into());
    task.ci_pr_number = Some(44);
    task.ci_blocking_required_check_names = r#"["Quality Gate","Server Tests"]"#.into();
    task.ci_failure_fingerprint = Some("lint+tests@failing1234567890".into());
    task.ci_first_seen_at = Some("2026-06-01T00:00:00Z".into());
    task.ci_last_seen_at = Some("2026-06-01T00:10:00Z".into());
    task.ci_same_signature_count = 3;
    task.ci_last_remediation_base_sha = Some("base1234567890abc".into());

    let response = task_to_response(&task);
    let serialized = serde_json::to_value(&response).unwrap();
    let ci = &serialized["ci"];

    assert_eq!(serialized["status"], "closed");
    assert_eq!(serialized["ci_status"], "failing");
    assert_eq!(serialized["ci_gate_state"], "failing");
    assert_eq!(serialized["ci_primary_blocking_check"], "Quality Gate");
    assert_eq!(
        serialized["ci_summary_reason"],
        "Required check failing: Quality Gate"
    );
    assert_eq!(
        serialized["ci_merge_blocked_reason"],
        "Blocked by failing required check: Quality Gate"
    );
    assert_eq!(ci["blocking_required_check_names"][0], "Quality Gate");
    assert_eq!(ci["blocking_required_check_names"][1], "Server Tests");
    assert_eq!(ci["failure_fingerprint"], "lint+tests@failing1234567890");
}

#[test]
fn regression_advisory_failures_do_not_block_when_required_ci_passes() {
    let mut task = task_with_merge_commit_sha(None);
    task.ci_status = "passing".into();
    task.ci_head_sha = Some("advisory1234567890".into());
    task.ci_pr_number = Some(46);
    task.ci_blocking_required_check_names = "[]".into();

    let serialized = serde_json::to_value(task_to_response(&task)).unwrap();
    let ci = &serialized["ci"];

    assert_eq!(ci["status"], "passing");
    assert_eq!(ci["gate_state"], "passing");
    assert_eq!(
        ci["blocking_required_check_names"]
            .as_array()
            .unwrap()
            .len(),
        0
    );
    assert!(ci.get("primary_blocking_check").is_none() || ci["primary_blocking_check"].is_null());
    assert!(ci.get("merge_blocked_reason").is_none() || ci["merge_blocked_reason"].is_null());
    assert!(
        serialized.get("ci_primary_blocking_check").is_none()
            || serialized["ci_primary_blocking_check"].is_null()
    );
    assert!(
        serialized.get("ci_merge_blocked_reason").is_none()
            || serialized["ci_merge_blocked_reason"].is_null()
    );
}
