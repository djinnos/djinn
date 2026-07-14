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
        ci_mq_state: None,
        ci_mq_run_id: None,
        ci_mq_head_sha: None,
        ci_mq_failed_check_names: None,
        ci_mq_failure_fingerprint: None,
        ci_mq_same_signature_count: None,
        ci_mq_first_seen_at: None,
        ci_mq_last_seen_at: None,
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

// ── CI head reconciliation fields (m116) ─────────────────────────────

/// Helper: a task with a basic CI snapshot (so `task_ci_gate_snapshot`
/// returns `Some`), no reconciliation fields set yet.
fn task_with_ci_snapshot_for_reconciliation() -> Task {
    let mut task = task_with_merge_commit_sha(None);
    task.ci_status = "failing".into();
    task.ci_head_sha = Some("deadbeefcafebabe00000000000000000000ffff".into());
    task.ci_pr_number = Some(42);
    task.ci_blocking_required_check_names = r#"["clippy"]"#.into();
    task
}

#[test]
fn ci_snapshot_without_reconciliation_fields_is_backwards_compatible() {
    // No mirror/github/divergence/error fields set — payload must look
    // identical to the pre-m116 shape for existing `head_sha` consumers.
    let task = task_with_ci_snapshot_for_reconciliation();
    let response = task_to_response(&task);
    let serialized = serde_json::to_value(&response).unwrap();
    let ci = serialized["ci"]
        .as_object()
        .expect("ci should be an object");

    // Existing head_sha consumer contract preserved.
    assert_eq!(ci["head_sha"], "deadbeefcafebabe00000000000000000000ffff");

    // New additive fields are absent (skip_serializing_if Option::is_none).
    assert!(
        ci.get("mirror_head_sha").is_none(),
        "mirror_head_sha must be absent when None"
    );
    assert!(
        ci.get("github_head_sha").is_none(),
        "github_head_sha must be absent when None"
    );
    assert!(
        ci.get("heads_diverged").is_none(),
        "heads_diverged must be absent when None"
    );
    assert!(
        ci.get("head_observation_error").is_none(),
        "head_observation_error must be absent when None"
    );
    // Merge-queue lane absent when no mq state recorded.
    assert!(
        ci.get("merge_queue").is_none(),
        "merge_queue must be absent when no lane state is recorded"
    );
}

#[test]
fn ci_snapshot_surfaces_merge_queue_lane_when_mq_columns_set() {
    let mut task = task_with_ci_snapshot_for_reconciliation();
    task.ci_mq_state = Some("dequeued_failure".into());
    task.ci_mq_run_id = Some(778899);
    task.ci_mq_head_sha = Some("mq00head00sha000000000000000000000000ffff".into());
    task.ci_mq_failed_check_names = Some(r#"["Integration Tests","Server Tests"]"#.into());
    task.ci_mq_failure_fingerprint = Some("mq-fp-abc".into());
    task.ci_mq_same_signature_count = Some(3);
    task.ci_mq_first_seen_at = Some("2026-07-14T00:00:00.000Z".into());
    task.ci_mq_last_seen_at = Some("2026-07-14T00:05:00.000Z".into());

    let serialized = serde_json::to_value(task_to_response(&task)).unwrap();
    let lane = serialized["ci"]["merge_queue"]
        .as_object()
        .expect("merge_queue lane should be an object when mq columns are set");

    assert_eq!(lane["state"], "dequeued_failure");
    assert_eq!(lane["run_id"], 778899);
    assert_eq!(
        lane["head_sha"],
        "mq00head00sha000000000000000000000000ffff"
    );
    assert_eq!(
        lane["failed_check_names"],
        serde_json::json!(["Integration Tests", "Server Tests"])
    );
    assert_eq!(lane["failure_fingerprint"], "mq-fp-abc");
    assert_eq!(lane["same_signature_count"], 3);
    assert_eq!(lane["first_seen_at"], "2026-07-14T00:00:00.000Z");
    assert_eq!(lane["last_seen_at"], "2026-07-14T00:05:00.000Z");
}

#[test]
fn ci_snapshot_merge_queue_lane_omits_absent_optional_fields() {
    let mut task = task_with_ci_snapshot_for_reconciliation();
    // Only the state is known; every other lane field is nullable.
    task.ci_mq_state = Some("dequeued_failure".into());
    task.ci_mq_same_signature_count = Some(1);

    let serialized = serde_json::to_value(task_to_response(&task)).unwrap();
    let lane = serialized["ci"]["merge_queue"]
        .as_object()
        .expect("merge_queue lane present with only state set");

    assert_eq!(lane["state"], "dequeued_failure");
    assert_eq!(lane["same_signature_count"], 1);
    // failed_check_names defaults to an empty array (always serialized).
    assert_eq!(lane["failed_check_names"], serde_json::json!([]));
    for absent in [
        "run_id",
        "head_sha",
        "failure_fingerprint",
        "first_seen_at",
        "last_seen_at",
    ] {
        assert!(
            lane.get(absent).is_none(),
            "{absent} must be omitted when None"
        );
    }
}

#[test]
fn ci_snapshot_equal_heads_serialize_diverged_false() {
    let mut task = task_with_ci_snapshot_for_reconciliation();
    task.ci_mirror_head_sha = Some("abc123abc123abc123abc123abc123abc123abcd".into());
    task.ci_github_head_sha = Some("abc123abc123abc123abc123abc123abc123abcd".into());
    task.ci_heads_diverged = Some(false);

    let serialized = serde_json::to_value(task_to_response(&task)).unwrap();
    let ci = &serialized["ci"];

    assert_eq!(
        ci["mirror_head_sha"],
        "abc123abc123abc123abc123abc123abc123abcd"
    );
    assert_eq!(
        ci["github_head_sha"],
        "abc123abc123abc123abc123abc123abc123abcd"
    );
    assert_eq!(ci["heads_diverged"], false);
    // No observation error.
    assert!(ci.get("head_observation_error").is_none());
}

#[test]
fn ci_snapshot_diverged_heads_serialize_diverged_true() {
    let mut task = task_with_ci_snapshot_for_reconciliation();
    task.ci_mirror_head_sha = Some("mirror111111111111111111111111111111111111".into());
    task.ci_github_head_sha = Some("github222222222222222222222222222222222222".into());
    task.ci_heads_diverged = Some(true);

    let serialized = serde_json::to_value(task_to_response(&task)).unwrap();
    let ci = &serialized["ci"];

    assert_eq!(
        ci["mirror_head_sha"],
        "mirror111111111111111111111111111111111111"
    );
    assert_eq!(
        ci["github_head_sha"],
        "github222222222222222222222222222222222222"
    );
    assert_eq!(ci["heads_diverged"], true);
}

#[test]
fn ci_snapshot_unknown_mirror_head_leaves_diverged_absent() {
    let mut task = task_with_ci_snapshot_for_reconciliation();
    // GitHub head known but mirror head unknown.
    task.ci_mirror_head_sha = None;
    task.ci_github_head_sha = Some("github222222222222222222222222222222222222".into());
    task.ci_heads_diverged = None; // cannot determine divergence

    let serialized = serde_json::to_value(task_to_response(&task)).unwrap();
    let ci = &serialized["ci"];

    assert!(ci.get("mirror_head_sha").is_none());
    assert_eq!(
        ci["github_head_sha"],
        "github222222222222222222222222222222222222"
    );
    // heads_diverged must be absent/null-compatible.
    assert!(
        ci.get("heads_diverged").is_none(),
        "heads_diverged must be absent when mirror head is unknown"
    );
}

#[test]
fn ci_snapshot_unknown_github_head_leaves_diverged_absent() {
    let mut task = task_with_ci_snapshot_for_reconciliation();
    // Mirror head known but GitHub head unknown (e.g. no open PR branch).
    task.ci_mirror_head_sha = Some("mirror111111111111111111111111111111111111".into());
    task.ci_github_head_sha = None;
    task.ci_heads_diverged = None;

    let serialized = serde_json::to_value(task_to_response(&task)).unwrap();
    let ci = &serialized["ci"];

    assert_eq!(
        ci["mirror_head_sha"],
        "mirror111111111111111111111111111111111111"
    );
    assert!(ci.get("github_head_sha").is_none());
    assert!(
        ci.get("heads_diverged").is_none(),
        "heads_diverged must be absent when github head is unknown"
    );
}

#[test]
fn ci_snapshot_head_observation_error_serializes_when_present() {
    let mut task = task_with_ci_snapshot_for_reconciliation();
    task.ci_mirror_head_sha = Some("mirror111111111111111111111111111111111111".into());
    task.ci_github_head_sha = None;
    task.ci_heads_diverged = None;
    task.ci_head_observation_error = Some("GitHub push failed: 422 Validation Failed".into());

    let serialized = serde_json::to_value(task_to_response(&task)).unwrap();
    let ci = &serialized["ci"];

    assert_eq!(
        ci["head_observation_error"],
        "GitHub push failed: 422 Validation Failed"
    );
}

#[test]
fn ci_snapshot_reconciliation_fields_in_list_item() {
    let mut task = task_with_ci_snapshot_for_reconciliation();
    task.ci_mirror_head_sha = Some("aaaabbbbccccddddeeeeffff000011112222".into());
    task.ci_github_head_sha = Some("1111222233334444555566667777888899990".into());
    task.ci_heads_diverged = Some(true);

    let list_item = task_to_list_item(&task, None, 0);
    let serialized = serde_json::to_value(&list_item).unwrap();
    let ci = &serialized["ci"];

    assert_eq!(
        ci["mirror_head_sha"],
        "aaaabbbbccccddddeeeeffff000011112222"
    );
    assert_eq!(
        ci["github_head_sha"],
        "1111222233334444555566667777888899990"
    );
    assert_eq!(ci["heads_diverged"], true);
}

// ── Forward-compatible consumer simulation (m116) ──────────────────────

/// Simulates a consumer that only knows about pre-m116 `CiGateSnapshot`
/// fields.  When the JSON payload contains the new m116 reconciliation
/// fields, the consumer's partial deserialization must succeed and the
/// old `head_sha` value must be intact.  This is the durability contract:
/// additive nullable fields do not break existing consumers.
#[test]
fn forward_compatible_consumer_ignores_new_reconciliation_fields() {
    // A struct representing a pre-m116 consumer's view of the CI payload.
    // It only knows about `head_sha` and `status`.
    #[derive(serde::Deserialize)]
    struct LegacyCiConsumer {
        head_sha: String,
        status: String,
    }

    // Build a task with ALL reconciliation fields populated.
    let mut task = task_with_ci_snapshot_for_reconciliation();
    task.ci_mirror_head_sha = Some("mirror111111111111111111111111111111111111".into());
    task.ci_github_head_sha = Some("github222222222222222222222222222222222222".into());
    task.ci_heads_diverged = Some(true);
    task.ci_head_observation_error = Some("push failed".into());

    let response = task_to_response(&task);
    let serialized = serde_json::to_value(&response).unwrap();
    let ci_value = &serialized["ci"];

    // A legacy consumer deserializes only the fields it knows about.
    let legacy: LegacyCiConsumer =
        serde_json::from_value(ci_value.clone()).expect("legacy consumer must parse");

    assert_eq!(
        legacy.head_sha, "deadbeefcafebabe00000000000000000000ffff",
        "head_sha must be preserved for legacy consumers"
    );
    assert_eq!(legacy.status, "failing");

    // The full payload still has the new fields — they coexist.
    assert_eq!(ci_value["heads_diverged"], true);
    assert_eq!(ci_value["head_observation_error"], "push failed");
}

/// Same forward-compatibility check via `task_list_item`, confirming the
/// list path also carries new fields without breaking legacy consumers.
#[test]
fn forward_compatible_list_consumer_ignores_new_reconciliation_fields() {
    #[derive(serde::Deserialize)]
    struct LegacyListItemCi {
        head_sha: String,
    }

    let mut task = task_with_ci_snapshot_for_reconciliation();
    task.ci_mirror_head_sha = Some("m1".into());
    task.ci_github_head_sha = Some("g1".into());
    task.ci_heads_diverged = Some(true);
    task.ci_head_observation_error = Some("err".into());

    let list_item = task_to_list_item(&task, None, 0);
    let serialized = serde_json::to_value(&list_item).unwrap();
    let ci_value = &serialized["ci"];

    let legacy: LegacyListItemCi =
        serde_json::from_value(ci_value.clone()).expect("legacy list consumer must parse");

    assert_eq!(legacy.head_sha, "deadbeefcafebabe00000000000000000000ffff");
    assert_eq!(ci_value["heads_diverged"], true);
}
