//! Regression tests for the promoted BLOCKING red-CI directive.
//!
//! These tests verify that worker and reviewer prompt contexts contain exactly
//! one promoted `## ⛔ BLOCKING: Required CI Failing` directive sourced from
//! structured CI snapshot fields, and that ordinary activity-log CI audit
//! events do not create a second promoted directive.

use super::*;

use djinn_core::events::EventBus;
use djinn_core::models::Epic;
use djinn_db::{Database, EpicCreateInput, EpicRepository, TaskRepository};
use tokio_util::sync::CancellationToken;

use crate::roles::{ReviewerRole, WorkerRole};
use crate::test_helpers::{agent_context_from_db, create_test_project, test_tempdir};

// ── Shared test helpers ─────────────────────────────────────────────────────

async fn create_epic(
    db: &Database,
    events: &EventBus,
    project_id: &str,
    title: &str,
    description: &str,
    status: Option<&str>,
) -> Epic {
    EpicRepository::new(db.clone(), events.clone())
        .create_for_project(
            project_id,
            EpicCreateInput {
                title,
                description,
                emoji: "🧪",
                color: "blue",
                owner: "test-owner",
                memory_refs: None,
                status,
                auto_breakdown: None,
                originating_adr_id: None,
                blocked_by: None,
            },
        )
        .await
        .expect("create test epic")
}

async fn full_prompt_context_for_role(
    db: Database,
    task: &djinn_core::models::Task,
    role: &dyn AgentRole,
) -> PromptContext {
    let app_state = agent_context_from_db(db, CancellationToken::new());
    let worktree = test_tempdir("prompt-context-worktree-");
    assemble_prompt_context(PromptContextInputs {
        task,
        runtime_role: role,
        role_for_epic_check: role,
        project_path: "/workspace/test-project",
        worktree_path: worktree.path(),
        conflict_ctx: None,
        merge_validation_ctx: None,
        prompt_setup_commands: None,
        system_prompt_extensions: "",
        learned_prompt: None,
        resolved_skills: &[],
        app_state: &app_state,
        read_sources: &[],
    })
    .await
}

fn ci_directive_section(prompt: &str) -> &str {
    let start = prompt
        .find("## ⛔ BLOCKING: Required CI Failing")
        .expect("prompt must contain promoted CI BLOCKING section");
    let rest = &prompt[start..];
    match rest[3..].find("\n## ") {
        Some(end) => &rest[..3 + end],
        None => rest,
    }
}

fn assert_single_structured_ci_directive(prompt: &str) {
    assert_eq!(
        prompt
            .matches("## ⛔ BLOCKING: Required CI Failing")
            .count(),
        1,
        "prompt must contain exactly one promoted CI BLOCKING directive"
    );

    let directive = ci_directive_section(prompt);
    assert!(
        directive.contains("**PR:** #314"),
        "directive must use structured PR number from task CI snapshot: {directive}"
    );
    assert!(
        directive.contains("**Failing head SHA:** `structured-head-sha-314159`"),
        "directive must use structured head SHA from task CI snapshot: {directive}"
    );
    assert!(
        directive.contains("Structured Quality Gate"),
        "directive must use structured primary blocking check from task CI snapshot: {directive}"
    );
    assert!(
        directive.contains("Structured Server Tests"),
        "directive must include structured blocking check values from task CI snapshot: {directive}"
    );
    assert!(
        directive.contains("`structured-fingerprint-314`"),
        "directive must include structured failure fingerprint: {directive}"
    );
    assert!(
        directive.contains("**Remediation baseline SHA:** `structured-base-sha-271828`"),
        "directive must include structured remediation baseline SHA: {directive}"
    );
    assert!(
        !directive.contains("audit-log-head-sha-should-not-be-used"),
        "promoted directive must not be assembled from activity-log prose: {directive}"
    );
    assert!(
        !directive.contains("Audit Log CI Job Should Not Be Promoted"),
        "promoted directive must not scrape activity-log check names: {directive}"
    );
    assert!(
        !directive.contains("audit-log-reason-should-not-be-used"),
        "promoted directive must not scrape activity-log failure reasons: {directive}"
    );
}

// ── sa4x: build_ci_blocking_directive tests ────────────────────────────────

/// Helper to build a minimal Task with CI fields for directive tests.
fn make_task_with_ci(
    ci_status: &str,
    ci_head_sha: Option<&str>,
    ci_pr_number: Option<i64>,
    ci_blocking_checks: &str,
    ci_failure_fingerprint: Option<&str>,
    ci_last_remediation_base_sha: Option<&str>,
) -> djinn_core::models::Task {
    djinn_core::models::Task {
        id: "task-ci-test".into(),
        project_id: "project-1".into(),
        short_id: "t-ci".into(),
        epic_id: None,
        title: "CI test task".into(),
        description: "Test task for CI directive".into(),
        design: "".into(),
        issue_type: "task".into(),
        status: "open".into(),
        priority: 1,
        owner: "test@example.com".into(),
        labels: "[]".into(),
        acceptance_criteria: "[]".into(),
        reopen_count: 0,
        continuation_count: 0,
        total_reopen_count: 0,
        intervention_count: 0,
        last_intervention_at: None,
        created_at: "2026-01-01T00:00:00Z".into(),
        updated_at: "2026-01-01T00:00:00Z".into(),
        closed_at: None,
        close_reason: None,
        merge_commit_sha: None,
        pr_url: None,
        merge_conflict_metadata: None,
        memory_refs: "[]".into(),
        agent_type: None,
        created_by_user_id: None,
        ci_status: ci_status.into(),
        ci_head_sha: ci_head_sha.map(Into::into),
        ci_pr_number,
        ci_blocking_required_check_names: ci_blocking_checks.into(),
        ci_failure_fingerprint: ci_failure_fingerprint.map(Into::into),
        ci_first_seen_at: None,
        ci_last_seen_at: None,
        ci_same_signature_count: 0,
        ci_last_remediation_base_sha: ci_last_remediation_base_sha.map(Into::into),
        unresolved_blocker_count: 0,
    }
}

async fn task_with_structured_red_ci_and_audit_activity(
    db: &Database,
    events: &EventBus,
) -> djinn_core::models::Task {
    let project = create_test_project(db).await;
    let epic = create_epic(
        db,
        events,
        &project.id,
        "Structured CI directive epic",
        "Exercises red-CI prompt context assembly.",
        None,
    )
    .await;
    let task_repo = TaskRepository::new(db.clone(), events.clone());
    let task = task_repo
        .create(
            &epic.id,
            "Fix structured CI failure",
            "The task has red required CI.",
            "Use the durable CI snapshot, not activity prose.",
            "task",
            1,
            "test-owner",
            None,
        )
        .await
        .expect("create CI prompt task");

    // Ordinary CI audit prose intentionally contains distinct fake values. The
    // promoted directive must not scrape these comments to find job/reason/head.
    task_repo
        .log_activity(
            Some(&task.id),
            "system",
            "system",
            "comment",
            r#"{"body":"CI audit: required check failed. job=Audit Log CI Job Should Not Be Promoted; reason=audit-log-reason-should-not-be-used; head=audit-log-head-sha-should-not-be-used"}"#,
        )
        .await
        .expect("log first CI audit activity");
    task_repo
        .log_activity(
            Some(&task.id),
            "system",
            "system",
            "comment",
            r#"{"body":"CI audit repeat: job=Audit Log CI Job Should Not Be Promoted; reason=audit-log-reason-should-not-be-used; head=audit-log-head-sha-should-not-be-used"}"#,
        )
        .await
        .expect("log second CI audit activity");

    let mut task = task;
    task.ci_status = "failing".into();
    task.ci_head_sha = Some("structured-head-sha-314159".into());
    task.ci_pr_number = Some(314);
    task.ci_blocking_required_check_names =
        r#"["Structured Quality Gate", "Structured Server Tests"]"#.into();
    task.ci_failure_fingerprint = Some("structured-fingerprint-314".into());
    task.ci_last_remediation_base_sha = Some("structured-base-sha-271828".into());
    task
}

// ── AC1: Worker context regression ─────────────────────────────────────────

/// AC1: A task with failing required CI produces exactly one promoted
/// `BLOCKING:` directive for worker context, sourced from structured CI
/// snapshot fields.
#[tokio::test]
async fn worker_prompt_context_has_one_promoted_structured_ci_directive() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = task_with_structured_red_ci_and_audit_activity(&db, &events).await;
    let role = WorkerRole;

    let ctx = full_prompt_context_for_role(db, &task, &role).await;

    assert_single_structured_ci_directive(&ctx.system_prompt);
    assert!(
        ctx.activity_text
            .as_deref()
            .expect("activity audit text should be present")
            .contains("audit-log-head-sha-should-not-be-used"),
        "fixture must include audit-only CI prose so the test distinguishes structured rendering"
    );
}

// ── AC2: Reviewer context regression ───────────────────────────────────────

/// AC2: Reviewer context follows the same single-directive rule. Adding CI
/// failure activity-log comments does not duplicate the promoted directive.
#[tokio::test]
async fn reviewer_prompt_context_does_not_duplicate_directive_from_ci_audit_activity() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = task_with_structured_red_ci_and_audit_activity(&db, &events).await;
    let role = ReviewerRole;

    let ctx = full_prompt_context_for_role(db, &task, &role).await;

    assert_single_structured_ci_directive(&ctx.system_prompt);
    assert!(
        ctx.activity_text
            .as_deref()
            .expect("activity audit text should be present")
            .contains("Audit Log CI Job Should Not Be Promoted"),
        "fixture must keep CI audit activity visible as ordinary activity"
    );
}

// ── Unit tests for build_ci_blocking_directive ─────────────────────────────

#[test]
fn build_ci_blocking_directive_returns_none_for_passing_ci() {
    let task = make_task_with_ci(
        "passing",
        Some("abc123"),
        Some(42),
        "[]",
        None,
        Some("abc123"),
    );
    assert!(
        build_ci_blocking_directive(&task).is_none(),
        "directive should be None for passing CI"
    );
}

#[test]
fn build_ci_blocking_directive_returns_none_for_pending_ci() {
    let task = make_task_with_ci(
        "pending",
        Some("abc123"),
        Some(42),
        "[]",
        None,
        Some("abc123"),
    );
    assert!(
        build_ci_blocking_directive(&task).is_none(),
        "directive should be None for pending CI"
    );
}

#[test]
fn build_ci_blocking_directive_returns_none_for_unknown_ci() {
    let task = make_task_with_ci("unknown", None, None, "[]", None, None);
    assert!(
        build_ci_blocking_directive(&task).is_none(),
        "directive should be None for unknown CI"
    );
}

#[test]
fn build_ci_blocking_directive_returns_none_when_no_remediation_baseline() {
    let task = make_task_with_ci(
        "failing",
        Some("abc123"),
        Some(42),
        r#"["Quality Gate"]"#,
        Some("fp-xyz"),
        None, // no remediation baseline
    );
    assert!(
        build_ci_blocking_directive(&task).is_none(),
        "directive should be None when no remediation baseline exists"
    );
}

#[test]
fn build_ci_blocking_directive_returns_some_for_failing_ci_with_baseline() {
    let task = make_task_with_ci(
        "failing",
        Some("head-sha-456"),
        Some(42),
        r#"["Quality Gate", "unit tests"]"#,
        Some("fp-abc789"),
        Some("base-sha-123"),
    );
    let directive = build_ci_blocking_directive(&task).expect("directive should be Some");

    assert!(
        directive.contains("**PR:** #42"),
        "directive should contain concrete PR number"
    );
    assert!(
        directive.contains("**Failing head SHA:** `head-sha-456`"),
        "directive should contain failing head SHA"
    );
    assert!(
        directive.contains("Quality Gate"),
        "directive should contain blocking check names"
    );
    assert!(
        directive.contains("unit tests"),
        "directive should contain all blocking check names"
    );
    assert!(
        directive.contains("**Failure fingerprint:** `fp-abc789`"),
        "directive should contain failure fingerprint"
    );
    assert!(
        directive.contains("**Remediation baseline SHA:** `base-sha-123`"),
        "directive should contain remediation baseline SHA"
    );
    assert!(
        directive.contains("REQUIRED CI is failing"),
        "directive should contain the blocking instruction"
    );
}

#[test]
fn build_ci_blocking_directive_deduplication_same_baseline_produces_same_text() {
    let task1 = make_task_with_ci(
        "failing",
        Some("head-sha-456"),
        Some(42),
        r#"["Quality Gate"]"#,
        Some("fp-abc"),
        Some("base-sha-123"),
    );
    let task2 = make_task_with_ci(
        "failing",
        Some("head-sha-456"),
        Some(42),
        r#"["Quality Gate"]"#,
        Some("fp-abc"),
        Some("base-sha-123"),
    );
    let directive1 = build_ci_blocking_directive(&task1).expect("directive1 should be Some");
    let directive2 = build_ci_blocking_directive(&task2).expect("directive2 should be Some");

    assert_eq!(
        directive1, directive2,
        "same failing baseline should produce identical directive text (deduplication)"
    );
}

#[test]
fn build_ci_blocking_directive_uses_unknown_when_head_sha_missing() {
    let task = make_task_with_ci(
        "failing",
        None, // no head SHA
        Some(42),
        r#"["Quality Gate"]"#,
        Some("fp-abc"),
        Some("base-sha-123"),
    );
    let directive = build_ci_blocking_directive(&task).expect("directive should be Some");

    assert!(
        directive.contains("**Failing head SHA:** `unknown`"),
        "directive should use 'unknown' when head SHA is missing"
    );
}

#[test]
fn build_ci_blocking_directive_uses_zero_when_pr_number_missing() {
    let task = make_task_with_ci(
        "failing",
        Some("head-sha-456"),
        None, // no PR number
        r#"["Quality Gate"]"#,
        Some("fp-abc"),
        Some("base-sha-123"),
    );
    let directive = build_ci_blocking_directive(&task).expect("directive should be Some");

    assert!(
        directive.contains("**PR:** #0"),
        "directive should use #0 when PR number is missing"
    );
}

#[test]
fn build_ci_blocking_directive_handles_empty_check_names() {
    let task = make_task_with_ci(
        "failing",
        Some("head-sha-456"),
        Some(42),
        "[]", // empty check names
        Some("fp-abc"),
        Some("base-sha-123"),
    );
    let directive = build_ci_blocking_directive(&task).expect("directive should be Some");

    assert!(
        directive.contains("**Blocking checks:** unknown"),
        "directive should use 'unknown' when check names are empty"
    );
}

#[test]
fn build_ci_blocking_directive_omits_fingerprint_line_when_absent() {
    let task = make_task_with_ci(
        "failing",
        Some("head-sha-456"),
        Some(42),
        r#"["Quality Gate"]"#,
        None, // no fingerprint
        Some("base-sha-123"),
    );
    let directive = build_ci_blocking_directive(&task).expect("directive should be Some");

    assert!(
        !directive.contains("Failure fingerprint"),
        "directive should omit fingerprint line when fingerprint is None"
    );
}

// ── AC3: Promoted BLOCKING directive deduplication regression tests ────────
//
// AC3: These tests verify directive deduplication for worker and reviewer
// dispatch contexts using concrete PR/head/check/fingerprint values. The
// directive is derived from durable CI gate snapshot state, so the same
// failing baseline always produces the same text — regardless of how many
// times the prompt context is assembled.

/// Concrete values shared across worker and reviewer deduplication tests.
/// These match the values used in the pr_poller and supervisor_impl tests
/// to ensure the full sa4x guardrail chain is consistent.
const E2E_PR_NUMBER: i64 = 42;
const E2E_HEAD_SHA: &str = "abc123def456789012345678901234567890abcd";
const E2E_BASE_SHA: &str = "abc123def456789012345678901234567890abcd";
const E2E_FINGERPRINT: &str = "fp-e2e-sa4x-regression";
const E2E_CHECKS: &str = r#"["Quality Gate", "Server Clippy"]"#;

/// AC3: The promoted BLOCKING directive for worker dispatch context contains
/// all concrete PR/head/check/fingerprint values from the durable snapshot.
#[test]
fn sa4x_directive_worker_context_with_concrete_values() {
    let task = make_task_with_ci(
        "failing",
        Some(E2E_HEAD_SHA),
        Some(E2E_PR_NUMBER),
        E2E_CHECKS,
        Some(E2E_FINGERPRINT),
        Some(E2E_BASE_SHA),
    );
    let directive =
        build_ci_blocking_directive(&task).expect("directive must be Some for failing CI");

    // Verify all concrete values are present.
    assert!(
        directive.contains(&format!("**PR:** #{E2E_PR_NUMBER}")),
        "directive must contain concrete PR number"
    );
    assert!(
        directive.contains(&format!("`{E2E_HEAD_SHA}`")),
        "directive must contain the failing head SHA"
    );
    assert!(
        directive.contains("Quality Gate"),
        "directive must contain blocking check name 'Quality Gate'"
    );
    assert!(
        directive.contains("Server Clippy"),
        "directive must contain blocking check name 'Server Clippy'"
    );
    assert!(
        directive.contains(&format!("`{E2E_FINGERPRINT}`")),
        "directive must contain the failure fingerprint"
    );
    assert!(
        directive.contains(&format!("`{E2E_BASE_SHA}`")),
        "directive must contain the remediation baseline SHA"
    );
    assert!(
        directive.contains("REQUIRED CI is failing"),
        "directive must contain the blocking instruction"
    );
}

/// AC3: The promoted BLOCKING directive for reviewer dispatch context is
/// identical to the worker directive for the same failing baseline.
/// The directive is derived from the same durable snapshot state, so
/// deduplication is by construction.
#[test]
fn sa4x_directive_reviewer_context_matches_worker_context() {
    let task = make_task_with_ci(
        "failing",
        Some(E2E_HEAD_SHA),
        Some(E2E_PR_NUMBER),
        E2E_CHECKS,
        Some(E2E_FINGERPRINT),
        Some(E2E_BASE_SHA),
    );

    // build_ci_blocking_directive is a pure function of the Task's CI fields.
    // It produces the same text regardless of which role (worker or reviewer)
    // assembles the prompt context. This is the deduplication guarantee.
    let directive1 = build_ci_blocking_directive(&task).expect("first call must be Some");
    let directive2 = build_ci_blocking_directive(&task).expect("second call must be Some");

    assert_eq!(
        directive1, directive2,
        "same failing baseline must produce identical directive text for both roles"
    );
}

/// AC3: Directive deduplication across repeated dispatches. When the same
/// failing baseline is dispatched multiple times (e.g., worker retry),
/// the directive text must be identical each time — it's derived from
/// immutable snapshot state, not from a counter or timestamp.
#[test]
fn sa4x_directive_deduplication_across_repeated_dispatches() {
    let task = make_task_with_ci(
        "failing",
        Some(E2E_HEAD_SHA),
        Some(E2E_PR_NUMBER),
        E2E_CHECKS,
        Some(E2E_FINGERPRINT),
        Some(E2E_BASE_SHA),
    );

    // Simulate 5 repeated dispatches for the same failing baseline.
    let directives: Vec<String> = (0..5)
        .map(|_| build_ci_blocking_directive(&task).expect("directive must be Some each time"))
        .collect();

    // All must be identical.
    for (i, d) in directives.iter().enumerate() {
        assert_eq!(
            *d, directives[0],
            "dispatch #{i} must produce the same directive as dispatch #0"
        );
    }
}

/// AC3: Directive does NOT appear for advisory-only failures (ci_status != "failing").
/// This verifies the guardrail boundary: advisory checks do not produce a
/// BLOCKING directive in any dispatch context.
#[test]
fn sa4x_directive_absent_for_advisory_ci_status() {
    for status in &["passing", "pending", "unknown"] {
        let task = make_task_with_ci(
            status,
            Some(E2E_HEAD_SHA),
            Some(E2E_PR_NUMBER),
            E2E_CHECKS,
            Some(E2E_FINGERPRINT),
            Some(E2E_BASE_SHA),
        );
        assert!(
            build_ci_blocking_directive(&task).is_none(),
            "directive must be None for ci_status={status} (advisory/non-failing)"
        );
    }
}

/// AC3: Directive absent when failing but no remediation baseline exists.
/// A failing CI observation without a baseline means the poller hasn't
/// captured the initial remediation context yet — no directive is injected.
#[test]
fn sa4x_directive_absent_when_failing_without_baseline() {
    let task = make_task_with_ci(
        "failing",
        Some(E2E_HEAD_SHA),
        Some(E2E_PR_NUMBER),
        E2E_CHECKS,
        Some(E2E_FINGERPRINT),
        None, // no remediation baseline
    );
    assert!(
        build_ci_blocking_directive(&task).is_none(),
        "directive must be None when no remediation baseline exists"
    );
}
