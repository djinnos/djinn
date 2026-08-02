//! Regression tests for the promoted BLOCKING red-CI directive.

use super::*;

use djinn_core::events::EventBus;
use djinn_db::{Database, EffectiveCreatorProvenance, TaskRepository};

use crate::roles::{AgentRole, ReviewerRole, WorkerRole};

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
    let creator = crate::test_helpers::create_test_creator(db).await;
    let task = task_repo
        .create_in_project_with_provenance(
            &project.id,
            Some(&epic.id),
            EffectiveCreatorProvenance {
                explicit_user_id: Some(&creator.id),
                source_task_id: None,
                proposal_id: None,
            },
            "Fix structured CI failure",
            "The task has red required CI.",
            "Use the durable CI snapshot, not activity prose.",
            "task",
            1,
            "test-owner",
            None,
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
    let _knowledge_context_env = knowledge_context_test_env_guard();
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
            vec!["**Blocking checks (ranked, most causal first):** unknown"],
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

// ── Merge-queue (`merge_group`) lane directive ────────────────────────────────

/// Attach a merge-queue failure lane to a CI test task.
fn with_mq_lane(
    mut task: djinn_core::models::Task,
    state: &str,
    run_id: Option<i64>,
    failed_checks_json: &str,
    fingerprint: Option<&str>,
    same_signature_count: i64,
) -> djinn_core::models::Task {
    task.ci_mq_state = Some(state.into());
    task.ci_mq_run_id = run_id;
    task.ci_mq_head_sha = Some("mq-head-sha".into());
    task.ci_mq_failed_check_names = Some(failed_checks_json.into());
    task.ci_mq_failure_fingerprint = fingerprint.map(Into::into);
    task.ci_mq_same_signature_count = Some(same_signature_count);
    task
}

#[test]
fn build_ci_blocking_directive_renders_merge_queue_lane_on_green_head() {
    // Green PR head, but the merge queue rejected it at dequeue time → the
    // directive is still Some and renders the merge-queue section.
    let task = with_mq_lane(
        task_with_ci(
            "passing",
            Some("head-green"),
            Some(77),
            "[]",
            None,
            Some("base"),
        ),
        "dequeued_failure",
        Some(9988),
        r#"["Integration Tests", "Server Tests"]"#,
        Some("mq-fp-xyz"),
        3,
    );
    let directive = build_ci_blocking_directive(&task)
        .expect("merge-queue lane must produce a directive even on a green PR head");
    assert_contains_all(
        &directive,
        &[
            "**Merge-queue state:** dequeued_failure",
            "**Merge-group run:** `9988`",
            "Integration Tests",
            "Server Tests",
            "**Merge-queue failure fingerprint:** `mq-fp-xyz`",
            "3rd consecutive same-signature",
            "merge queue",
        ],
    );
    assert!(
        !directive.contains("REQUIRED CI is failing"),
        "green PR head must not render the PR-head failing section: {directive}"
    );
}

#[test]
fn build_ci_blocking_directive_combines_head_and_merge_queue_sections() {
    let task = with_mq_lane(
        task_with_ci(
            "failing",
            Some("head-red"),
            Some(77),
            r#"["Quality Gate"]"#,
            Some("fp-head"),
            Some("base"),
        ),
        "dequeued_failure",
        Some(1234),
        r#"["Integration Tests"]"#,
        Some("mq-fp"),
        1,
    );
    let directive = build_ci_blocking_directive(&task).expect("both sections present");
    assert_contains_all(
        &directive,
        &[
            "REQUIRED CI is failing",
            "**Remediation baseline SHA:** `base`",
            "**Merge-queue state:** dequeued_failure",
            "1st consecutive same-signature",
            "Integration Tests",
        ],
    );
}

#[test]
fn build_ci_blocking_directive_merge_queue_optional_fields_degrade_gracefully() {
    // Missing run id and fingerprint → "unknown"/omitted, no panic.
    let task = with_mq_lane(
        task_with_ci("passing", Some("h"), Some(1), "[]", None, Some("b")),
        "dequeued_failure",
        None,
        "[]",
        None,
        1,
    );
    let directive = build_ci_blocking_directive(&task).expect("mq lane present");
    assert_contains_all(
        &directive,
        &[
            "**Merge-group run:** unknown",
            "**Merge-group failing checks:** unknown",
        ],
    );
    assert!(
        !directive.contains("Merge-queue failure fingerprint"),
        "missing merge-queue fingerprint line must be omitted: {directive}"
    );
}

// ── wnqw: triage signal reaches the worker prompt ────────────────────────────
//
// Built from GitHub Actions run 30087861197 on PR #2525: a runner host filled
// its own disk, the fail-fast watcher cancelled the whole run, and the board
// pointed six sessions of agents at a never-executed aggregator.

/// The tlu1 cascade as it lands in the durable snapshot: ranked check list,
/// the ranked primary, and the annotation carrying the real cause.
fn tlu1_task() -> djinn_core::models::Task {
    let mut task = task_with_ci(
        "failing",
        Some("cbe3b7034deadbeef"),
        Some(2525),
        r#"["Plan Server Test Shards","Quality Gate","Server Test (shard-1, 0)","Publish Nextest Timing"]"#,
        Some("fp-tlu1"),
        Some("base-tlu1"),
    );
    task.ci_primary_blocking_check = Some("Plan Server Test Shards".into());
    task.ci_failure_annotations = Some(
        "Annotations on `Plan Server Test Shards`:\n\
         - [failure] System.IO.IOException: No space left on device : \
         '/home/runner/actions-runner/cached/2.336.0/_diag/Worker_20260724-105745-utc.log'"
            .into(),
    );
    task
}

#[test]
fn failing_directive_names_the_ranked_lane_and_shows_its_annotation() {
    let directive = build_ci_blocking_directive(&tlu1_task()).expect("directive must be Some");

    assert_contains_all(
        &directive,
        &[
            "**Start here:** `Plan Server Test Shards`",
            // The whole point: the cause is legible without opening GitHub.
            "No space left on device",
            "_diag/Worker_20260724-105745-utc.log",
            // And the worker is told what to do with an infra failure.
            "context deadline exceeded",
            "docker pull failed",
            "toomanyrequests",
            "registry 5xx",
            "TLS/handshake timeout",
            "Initialize containers",
            "retrigger CI and report the infrastructure failure",
        ],
    );
}

#[test]
fn failing_directive_omits_the_start_here_line_when_nothing_was_ranked() {
    let mut task = tlu1_task();
    task.ci_primary_blocking_check = None;
    task.ci_failure_annotations = None;

    let directive = build_ci_blocking_directive(&task).expect("directive must be Some");

    assert!(
        !directive.contains("**Start here:**"),
        "no ranked lane means no lane to point at: {directive}"
    );
    assert!(
        !directive.contains("```"),
        "no annotations means no empty evidence block: {directive}"
    );
}

#[test]
fn inconclusive_directive_forbids_remediation_and_names_no_lane() {
    let mut task = tlu1_task();
    task.ci_status = "inconclusive".into();
    task.ci_primary_blocking_check = None;
    task.ci_failure_fingerprint = None;
    // An inconclusive run has no remediation baseline; the directive must
    // render anyway, because silence would leave the worker with no guidance.
    task.ci_last_remediation_base_sha = None;

    let directive = build_ci_blocking_directive(&task).expect("inconclusive must still speak");

    assert_contains_all(
        &directive,
        &[
            "INCONCLUSIVE, not red",
            "Do NOT start a remediation attempt",
            "CI is being retriggered",
        ],
    );
    assert!(
        !directive.contains("You MUST fix"),
        "an inconclusive run must never issue a fix order: {directive}"
    );
    assert!(
        !directive.contains("**Start here:**"),
        "there is no causal lane to start from: {directive}"
    );
}

#[test]
fn inconclusive_directive_does_not_displace_a_real_failing_directive() {
    // `failing` still wins: the inconclusive section is a fallback for the
    // head lane, never an addition to it.
    let directive = build_ci_blocking_directive(&tlu1_task()).expect("directive must be Some");
    assert!(directive.contains("REQUIRED CI is failing"));
    assert!(!directive.contains("INCONCLUSIVE, not red"));
}
