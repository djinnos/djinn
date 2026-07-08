//! Regression tests for the promoted BLOCKING red-CI directive.

use super::*;

use djinn_core::events::EventBus;
use djinn_db::{Database, TaskRepository};

use crate::roles::{ReviewerRole, WorkerRole};

use super::test_support::{assemble_for_role, assert_contains_all, create_epic, task_with_ci};
use crate::test_helpers::create_test_project;

fn ci_directive_section(prompt: &str) -> &str {
    let start = prompt
        .find("## ⛔ BLOCKING: Required CI Failing")
        .expect("prompt must contain promoted CI BLOCKING section");
    let rest = &prompt[start..];
    rest[3..].find("\n## ").map_or(rest, |end| &rest[..3 + end])
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
    assert_contains_all(
        directive,
        &[
            "**PR:** #314",
            "**Failing head SHA:** `structured-head-sha-314159`",
            "Structured Quality Gate",
            "Structured Server Tests",
            "`structured-fingerprint-314`",
            "**Remediation baseline SHA:** `structured-base-sha-271828`",
        ],
    );
    for audit_only in [
        "audit-log-head-sha-should-not-be-used",
        "Audit Log CI Job Should Not Be Promoted",
        "audit-log-reason-should-not-be-used",
    ] {
        assert!(
            !directive.contains(audit_only),
            "promoted directive must not scrape activity-log prose: {directive}"
        );
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
    for body in [
        "CI audit: required check failed. job=Audit Log CI Job Should Not Be Promoted; reason=audit-log-reason-should-not-be-used; head=audit-log-head-sha-should-not-be-used",
        "CI audit repeat: job=Audit Log CI Job Should Not Be Promoted; reason=audit-log-reason-should-not-be-used; head=audit-log-head-sha-should-not-be-used",
    ] {
        task_repo
            .log_activity(
                Some(&task.id),
                "verification",
                "verification",
                "comment",
                &serde_json::json!({ "body": body }).to_string(),
            )
            .await
            .expect("log CI audit activity");
    }
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

#[tokio::test]
async fn prompt_context_has_one_promoted_structured_ci_directive_per_role() {
    for (role_name, role) in [
        ("worker", &WorkerRole as &dyn AgentRole),
        ("reviewer", &ReviewerRole as &dyn AgentRole),
    ] {
        let db = Database::ephemeral().await.expect("create ephemeral db");
        let events = EventBus::noop();
        let task = task_with_structured_red_ci_and_audit_activity(&db, &events).await;
        let ctx = assemble_for_role(db, &task, role, None, "", &[], &[]).await;
        assert_single_structured_ci_directive(&ctx.system_prompt);
        let activity = ctx
            .activity_text
            .as_deref()
            .expect("activity audit text should be present");
        assert!(
            activity.contains("audit-log-head-sha-should-not-be-used")
                && activity.contains("Audit Log CI Job Should Not Be Promoted"),
            "{role_name} fixture must keep CI audit prose visible as ordinary activity"
        );
    }
}

#[test]
fn build_ci_blocking_directive_absence_cases() {
    for (name, task) in [
        (
            "passing CI",
            task_with_ci(
                "passing",
                Some("abc123"),
                Some(42),
                "[]",
                None,
                Some("abc123"),
            ),
        ),
        (
            "advisory failure with passing required CI",
            task_with_ci(
                "passing",
                Some("advisory1234567890"),
                Some(46),
                "[]",
                None,
                Some("base-sha-from-pr"),
            ),
        ),
        (
            "pending CI",
            task_with_ci(
                "pending",
                Some("abc123"),
                Some(42),
                "[]",
                None,
                Some("abc123"),
            ),
        ),
        (
            "unknown CI",
            task_with_ci("unknown", None, None, "[]", None, None),
        ),
        (
            "failing CI without remediation baseline",
            task_with_ci(
                "failing",
                Some("abc123"),
                Some(42),
                r#"["Quality Gate"]"#,
                Some("fp-xyz"),
                None,
            ),
        ),
    ] {
        assert!(
            build_ci_blocking_directive(&task).is_none(),
            "directive should be None for {name}"
        );
    }
}

#[test]
fn build_ci_blocking_directive_renders_failing_snapshot_fields() {
    let directive = build_ci_blocking_directive(&task_with_ci(
        "failing",
        Some("head-sha-456"),
        Some(42),
        r#"["Quality Gate", "unit tests"]"#,
        Some("fp-abc789"),
        Some("base-sha-123"),
    ))
    .expect("directive should be Some");
    assert_contains_all(
        &directive,
        &[
            "**PR:** #42",
            "**Failing head SHA:** `head-sha-456`",
            "Quality Gate",
            "unit tests",
            "**Failure fingerprint:** `fp-abc789`",
            "**Remediation baseline SHA:** `base-sha-123`",
            "REQUIRED CI is failing",
        ],
    );
}

#[test]
fn build_ci_blocking_directive_default_and_optional_field_cases() {
    let cases = [
        (
            "missing head SHA",
            task_with_ci(
                "failing",
                None,
                Some(42),
                r#"["Quality Gate"]"#,
                Some("fp-abc"),
                Some("base-sha-123"),
            ),
            vec!["**Failing head SHA:** `unknown`"],
            vec![],
        ),
        (
            "missing PR number",
            task_with_ci(
                "failing",
                Some("head-sha-456"),
                None,
                r#"["Quality Gate"]"#,
                Some("fp-abc"),
                Some("base-sha-123"),
            ),
            vec!["**PR:** #0"],
            vec![],
        ),
        (
            "empty check names",
            task_with_ci(
                "failing",
                Some("head-sha-456"),
                Some(42),
                "[]",
                Some("fp-abc"),
                Some("base-sha-123"),
            ),
            vec!["**Blocking checks:** unknown"],
            vec![],
        ),
        (
            "missing fingerprint",
            task_with_ci(
                "failing",
                Some("head-sha-456"),
                Some(42),
                r#"["Quality Gate"]"#,
                None,
                Some("base-sha-123"),
            ),
            vec!["Quality Gate"],
            vec!["Failure fingerprint"],
        ),
    ];
    for (name, task, present, absent) in cases {
        let directive = build_ci_blocking_directive(&task).unwrap_or_else(|| panic!("{name}"));
        assert_contains_all(&directive, &present);
        for needle in absent {
            assert!(!directive.contains(needle), "{name}: unexpected {needle}");
        }
    }
}

const E2E_PR_NUMBER: i64 = 42;
const E2E_HEAD_SHA: &str = "abc123def456789012345678901234567890abcd";
const E2E_BASE_SHA: &str = "abc123def456789012345678901234567890abcd";
const E2E_FINGERPRINT: &str = "fp-e2e-sa4x-regression";
const E2E_CHECKS: &str = r#"["Quality Gate", "Server Clippy"]"#;

fn e2e_ci_task() -> djinn_core::models::Task {
    task_with_ci(
        "failing",
        Some(E2E_HEAD_SHA),
        Some(E2E_PR_NUMBER),
        E2E_CHECKS,
        Some(E2E_FINGERPRINT),
        Some(E2E_BASE_SHA),
    )
}

#[test]
fn sa4x_directive_contains_concrete_values_and_is_stable() {
    let task = e2e_ci_task();
    let directive = build_ci_blocking_directive(&task).expect("directive must be Some");
    assert_contains_all(
        &directive,
        &[
            &format!("**PR:** #{E2E_PR_NUMBER}"),
            E2E_HEAD_SHA,
            "Quality Gate",
            "Server Clippy",
            E2E_FINGERPRINT,
            E2E_BASE_SHA,
            "REQUIRED CI is failing",
        ],
    );
    for i in 0..5 {
        assert_eq!(
            directive,
            build_ci_blocking_directive(&task).expect("directive must be Some each time"),
            "dispatch #{i} must produce identical directive text"
        );
    }
}

#[test]
fn sa4x_directive_absent_for_advisory_statuses_or_missing_baseline() {
    for status in ["passing", "pending", "unknown"] {
        let task = task_with_ci(
            status,
            Some(E2E_HEAD_SHA),
            Some(E2E_PR_NUMBER),
            E2E_CHECKS,
            Some(E2E_FINGERPRINT),
            Some(E2E_BASE_SHA),
        );
        assert!(
            build_ci_blocking_directive(&task).is_none(),
            "directive must be None for ci_status={status}"
        );
    }
    assert!(
        build_ci_blocking_directive(&task_with_ci(
            "failing",
            Some(E2E_HEAD_SHA),
            Some(E2E_PR_NUMBER),
            E2E_CHECKS,
            Some(E2E_FINGERPRINT),
            None,
        ))
        .is_none()
    );
}
