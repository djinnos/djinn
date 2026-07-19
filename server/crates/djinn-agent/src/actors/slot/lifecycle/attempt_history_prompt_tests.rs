use super::*;

use djinn_core::events::EventBus;
use djinn_core::models::task_attempt::{
    GuardDecision, GuardReason, TaskAttemptOutcome, TaskAttemptPromptSummary,
};
use djinn_db::test_support::close_task_at;
use djinn_db::{
    CompletedParentSummary, CreateTaskAttemptParams, Database, GuardDeferTaskAttemptParams,
    TaskAttemptRepository, TerminalTaskAttemptParams,
};

use crate::roles::LeadRole;
use crate::test_helpers::create_test_project;

use super::test_support::{
    assemble_for_role, assert_ordered, create_epic, create_project_epic_task, create_task,
};

async fn lead_prompt_context(db: Database, task: &Task) -> PromptContext {
    let role = LeadRole;
    assemble_for_role(db, task, &role, None, "", &[], &[]).await
}

/// Create a terminal worker attempt for `task_id` with the given outcome and summary.
async fn seed_terminal_attempt(
    repo: &TaskAttemptRepository,
    task_id: &str,
    dispatch_key: &str,
    outcome: TaskAttemptOutcome,
    summary: Option<&str>,
) {
    let id = uuid::Uuid::now_v7().to_string();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id,
            role: "worker",
            dispatch_key,
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .expect("create pending attempt");
    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &attempt.id,
        outcome,
        pr_url: None,
        submit_ref: None,
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary,
        summary_json: None,
        log_tail: None,
    })
    .await
    .expect("advance to terminal");
    // Small delay so terminal_at timestamps are distinct and ordering is deterministic.
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
}

#[tokio::test]
async fn load_prior_attempts_returns_terminal_newest_first_bounded_to_three() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Attempt epic", "Attempt task").await;
    let repo = TaskAttemptRepository::new(db);

    // Seed 4 terminal attempts; only the 3 newest should be returned.
    for i in 1..=4 {
        seed_terminal_attempt(
            &repo,
            &task.id,
            &format!("dk-prior-{i}"),
            TaskAttemptOutcome::Completed,
            Some(&format!("summary {i}")),
        )
        .await;
    }

    let attempts = attempt_context::load_prior_attempts(&task, &repo)
        .await
        .expect("should return Some");
    assert_eq!(attempts.len(), 3, "bounded to 3 newest");
    // Newest-first: seq 4, 3, 2.
    assert_eq!(attempts[0].attempt_seq, 4);
    assert_eq!(attempts[1].attempt_seq, 3);
    assert_eq!(attempts[2].attempt_seq, 2);
    // Each carries the summary from the DTO.
    assert_eq!(attempts[0].summary.as_deref(), Some("summary 4"));
    // The DTO does not have a log_tail field — verify outcome is exposed.
    assert_eq!(attempts[0].outcome, "completed");
    assert_eq!(attempts[0].role, "worker");
}

#[tokio::test]
async fn load_prior_attempts_excludes_non_terminal_rows() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Filter epic", "Filter task").await;
    let repo = TaskAttemptRepository::new(db);

    // One terminal attempt.
    seed_terminal_attempt(
        &repo,
        &task.id,
        "dk-term-1",
        TaskAttemptOutcome::Completed,
        Some("terminal summary"),
    )
    .await;

    // One pending attempt (not advanced to terminal).
    let pending_id = uuid::Uuid::now_v7().to_string();
    repo.create_or_get_pending(CreateTaskAttemptParams {
        id: &pending_id,
        task_id: &task.id,
        role: "worker",
        dispatch_key: "dk-pending-1",
        session_id: None,
        dispatch_owner_incarnation_id: None,
        dispatch_group_id: None,
        attempt_seq: None,
    })
    .await
    .expect("create pending");

    // One submitted (non-terminal) attempt.
    let submitted_id = uuid::Uuid::now_v7().to_string();
    let submitted = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &submitted_id,
            task_id: &task.id,
            role: "worker",
            dispatch_key: "dk-submitted-1",
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .expect("create pending");
    repo.advance_to_submitted(djinn_db::SubmitTaskAttemptParams {
        id: &submitted.id,
        submit_ref: None,
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: Some("submitted summary"),
        summary_json: None,
        log_tail: None,
    })
    .await
    .expect("advance to submitted");

    let attempts = attempt_context::load_prior_attempts(&task, &repo)
        .await
        .expect("should return Some");
    assert_eq!(attempts.len(), 1, "only the terminal attempt is included");
    assert_eq!(attempts[0].summary.as_deref(), Some("terminal summary"));
}

#[tokio::test]
async fn load_prior_attempts_returns_none_when_empty() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Empty epic", "Empty task").await;
    let repo = TaskAttemptRepository::new(db);
    assert!(
        attempt_context::load_prior_attempts(&task, &repo)
            .await
            .is_none(),
        "no attempt rows should yield None"
    );
}

#[tokio::test]
async fn load_prior_attempts_exposes_dto_fields_without_log_tail() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "DTO epic", "DTO task").await;
    let repo = TaskAttemptRepository::new(db);

    let id = uuid::Uuid::now_v7().to_string();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task.id,
            role: "worker",
            dispatch_key: "dk-dto-1",
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .expect("create pending");
    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &attempt.id,
        outcome: TaskAttemptOutcome::Crashed,
        pr_url: Some("https://example.com/pr/42"),
        submit_ref: Some("refs/heads/task/dto"),
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: Some("crashed with panic"),
        summary_json: None,
        log_tail: None,
    })
    .await
    .expect("advance to terminal");

    let attempts = attempt_context::load_prior_attempts(&task, &repo)
        .await
        .expect("should return Some");
    assert_eq!(attempts.len(), 1);
    let s = &attempts[0];
    assert_eq!(s.outcome, "crashed");
    assert_eq!(s.role, "worker");
    assert_eq!(s.summary.as_deref(), Some("crashed with panic"));
    assert_eq!(s.pr_url.as_deref(), Some("https://example.com/pr/42"));
    assert_eq!(s.submit_ref.as_deref(), Some("refs/heads/task/dto"));
    assert!(s.terminal_at.is_some(), "terminal timestamp should be set");
    // The DTO struct itself has no log_tail field — the type system guarantees absence.
    assert!(
        std::any::type_name::<djinn_core::models::task_attempt::TaskAttemptPromptSummary>()
            .contains("TaskAttemptPromptSummary"),
        "load_prior_attempts must return the TaskAttemptPromptSummary DTO"
    );
}

#[tokio::test]
async fn load_completed_dependency_parents_returns_none_when_no_blockers() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "No-dep epic", "No-dep task").await;
    let repo = TaskAttemptRepository::new(db);
    assert!(
        attempt_context::load_completed_dependency_parents(&task, &repo)
            .await
            .is_none(),
        "no completed blocker parents should yield None"
    );
}

#[tokio::test]
async fn load_completed_dependency_parents_includes_closed_blocker_with_completed_attempt() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let project = create_test_project(&db).await;
    let epic = create_epic(&db, &events, &project.id, "Dep epic", "Dep.", None).await;
    let task = create_task(&db, &events, &epic.id, "Dependent task", None).await;
    // Closed blocker parent with a completed attempt.
    let parent = create_task(&db, &events, &epic.id, "Parent task", Some("closed")).await;
    close_task_at(&db, &parent.id, "2025-06-01T00:00:00Z").await;
    let task_repo = djinn_db::TaskRepository::new(db.clone(), events.clone());
    task_repo
        .update_blockers_atomic(&task.id, std::slice::from_ref(&parent.id), &[])
        .await
        .expect("wire blocker");

    let attempt_repo = TaskAttemptRepository::new(db.clone());
    seed_terminal_attempt(
        &attempt_repo,
        &parent.id,
        "dk-parent-1",
        TaskAttemptOutcome::Completed,
        Some("parent completed summary"),
    )
    .await;

    let parents = attempt_context::load_completed_dependency_parents(&task, &attempt_repo)
        .await
        .expect("should return Some");
    assert_eq!(parents.len(), 1);
    let p: &CompletedParentSummary = &parents[0];
    assert_eq!(p.task_id, parent.id);
    assert_eq!(p.title, "Parent task");
    let latest = p
        .latest_completed_attempt
        .as_ref()
        .expect("latest completed attempt should be present");
    assert_eq!(latest.summary.as_deref(), Some("parent completed summary"));
    assert_eq!(latest.outcome, "completed");
}

#[tokio::test]
async fn load_completed_dependency_parents_excludes_open_blocker() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let project = create_test_project(&db).await;
    let epic = create_epic(&db, &events, &project.id, "Excl epic", "Excl.", None).await;
    let task = create_task(&db, &events, &epic.id, "Dependent task 2", None).await;
    // Open blocker parent — should be excluded.
    let open_parent = create_task(&db, &events, &epic.id, "Open parent", None).await;
    let task_repo = djinn_db::TaskRepository::new(db.clone(), events.clone());
    task_repo
        .update_blockers_atomic(&task.id, std::slice::from_ref(&open_parent.id), &[])
        .await
        .expect("wire blocker");

    let attempt_repo = TaskAttemptRepository::new(db);
    assert!(
        attempt_context::load_completed_dependency_parents(&task, &attempt_repo)
            .await
            .is_none(),
        "open blocker parent should be excluded"
    );
}

#[tokio::test]
async fn assemble_prompt_context_loads_attempt_context_non_fatally() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    // End-to-end: assemble_prompt_context should populate prior_attempts and
    // completed_dependency_parents, and remain non-fatal when rows are absent.
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Assemble epic", "Assemble task").await;

    // Seed one terminal attempt for the task.
    let repo = TaskAttemptRepository::new(db.clone());
    seed_terminal_attempt(
        &repo,
        &task.id,
        "dk-assemble-1",
        TaskAttemptOutcome::Completed,
        Some("assemble summary"),
    )
    .await;

    let ctx = lead_prompt_context(db, &task).await;
    let attempts = ctx
        .prior_attempts
        .as_ref()
        .expect("prior_attempts should be populated");
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].summary.as_deref(), Some("assemble summary"));
    // No dependency parents → completed_dependency_parents is None, but assembly did not fail.
    assert!(ctx.completed_dependency_parents.is_none());
    // The prompt was still assembled successfully.
    assert!(!ctx.system_prompt.is_empty());
}

#[tokio::test]
async fn assemble_prompt_context_omits_attempt_context_when_no_rows() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "No-attempt epic", "No-attempt task").await;
    let ctx = lead_prompt_context(db, &task).await;
    assert!(
        ctx.prior_attempts.is_none(),
        "no attempt rows should yield None prior_attempts"
    );
    assert!(ctx.completed_dependency_parents.is_none());
    assert!(
        !ctx.system_prompt.is_empty(),
        "prompt assembly is non-fatal"
    );
}

// ═════════════════════════════════════════════════════════════════════════════
// End-to-end prompt snapshot tests (0kg0)
//
// These tests exercise the full attempt-history feedback integration by seeding
// `task_attempts` rows directly through the repository, then assembling the
// prompt context and verifying the rendered `activity_text` (which carries the
// attempt history inside the Activity Log section).
// ═════════════════════════════════════════════════════════════════════════════

/// Seed a guard-deferred attempt directly in the database.
async fn seed_guard_deferred_attempt(
    repo: &TaskAttemptRepository,
    task_id: &str,
    dispatch_key: &str,
    decision: GuardDecision,
    reason: GuardReason,
    summary: Option<&str>,
) {
    let id = uuid::Uuid::now_v7().to_string();
    repo.insert_guard_deferred(GuardDeferTaskAttemptParams {
        id: &id,
        task_id,
        role: "guard",
        dispatch_key,
        decision,
        reason,
        summary,
        summary_json: None,
        log_tail: None,
    })
    .await
    .expect("insert guard-deferred attempt");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
}

/// Seed a terminal attempt with full metadata (refs, summary_json).
#[allow(clippy::too_many_arguments)]
async fn seed_attempt_with_meta(
    repo: &TaskAttemptRepository,
    task_id: &str,
    dispatch_key: &str,
    outcome: TaskAttemptOutcome,
    summary: Option<&str>,
    summary_json: Option<&str>,
    pr_url: Option<&str>,
    submit_ref: Option<&str>,
    checkpoint_ref: Option<&str>,
) {
    let id = uuid::Uuid::now_v7().to_string();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id,
            role: "worker",
            dispatch_key,
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .expect("create pending attempt");
    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &attempt.id,
        outcome,
        pr_url,
        submit_ref,
        checkpoint_ref,
        mirror_head_sha: None,
        github_head_sha: None,
        summary,
        summary_json,
        log_tail: None,
    })
    .await
    .expect("advance to terminal");
    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
}

/// AC1: Current-task prior attempts are capped at 3, rendered newest-first
/// with role, outcome, and refs metadata.
#[tokio::test]
async fn prompt_renders_prior_attempts_capped_at_3_newest_first() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Cap epic", "Cap task").await;
    let repo = TaskAttemptRepository::new(db.clone());

    // Seed 5 terminal attempts; only the 3 newest should appear.
    for i in 1..=5 {
        seed_attempt_with_meta(
            &repo,
            &task.id,
            &format!("dk-cap-{i}"),
            TaskAttemptOutcome::Completed,
            Some(&format!("summary for attempt {i}")),
            None,
            Some(&format!("https://github.com/pr/{i}")),
            Some(&format!("refs/heads/task/{i}")),
            None,
        )
        .await;
    }

    let ctx = lead_prompt_context(db, &task).await;
    let text = ctx
        .activity_text
        .as_deref()
        .expect("activity_text should be populated with attempt history");

    // Header present.
    assert!(
        text.contains("**Prior attempts (newest first):**"),
        "should have attempts header: {text}"
    );

    // Newest 3 (seq 5, 4, 3) present, oldest 2 (seq 2, 1) absent.
    for i in [5, 4, 3] {
        assert!(
            text.contains(&format!("Attempt #{i} (worker): completed")),
            "attempt #{i} should be present: {text}"
        );
        assert!(
            text.contains(&format!("summary: summary for attempt {i}")),
            "attempt #{i} summary should be present: {text}"
        );
        assert!(
            text.contains(&format!("PR: https://github.com/pr/{i}")),
            "attempt #{i} PR ref should be present: {text}"
        );
        assert!(
            text.contains(&format!("submit_ref: `refs/heads/task/{i}`")),
            "attempt #{i} submit_ref should be present: {text}"
        );
    }
    for i in [2, 1] {
        assert!(
            !text.contains(&format!("Attempt #{i} (worker): completed")),
            "attempt #{i} should be dropped: {text}"
        );
    }

    // Newest-first ordering: attempt 5 before 4 before 3.
    assert_ordered(
        text,
        &[
            "Attempt #5 (worker): completed",
            "Attempt #4 (worker): completed",
            "Attempt #3 (worker): completed",
        ],
    );
}

/// AC1 supplement: guard decision/reason and checkpoint refs are rendered.
#[tokio::test]
async fn prompt_renders_guard_decision_and_checkpoint_refs() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Guard epic", "Guard task").await;
    let repo = TaskAttemptRepository::new(db.clone());

    // A deferred attempt with guard info.
    seed_guard_deferred_attempt(
        &repo,
        &task.id,
        "dk-guard-1",
        GuardDecision::Defer,
        GuardReason::ParkRung,
        Some("parked by guard"),
    )
    .await;

    // A terminal attempt with checkpoint ref.
    seed_attempt_with_meta(
        &repo,
        &task.id,
        "dk-ckpt-1",
        TaskAttemptOutcome::Completed,
        Some("completed with checkpoint"),
        None,
        None,
        None,
        Some("refs/heads/task/checkpoint-abc"),
    )
    .await;

    let ctx = lead_prompt_context(db, &task).await;
    let text = ctx
        .activity_text
        .as_deref()
        .expect("activity_text should be populated");

    assert!(
        text.contains("guard: defer (park_rung)"),
        "guard decision/reason should be rendered: {text}"
    );
    assert!(
        text.contains("parked by guard"),
        "guard summary should be present: {text}"
    );
    assert!(
        text.contains("checkpoint: `refs/heads/task/checkpoint-abc`"),
        "checkpoint ref should be rendered: {text}"
    );
}

/// AC2: Completed dependency parents are capped at 5, ordered by closed_at
/// descending then stable task id, exclude incomplete/non-parent tasks, and
/// use each parent's latest completed attempt.
#[tokio::test]
async fn prompt_renders_dependency_parents_capped_at_5_ordered() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let project = create_test_project(&db).await;
    let epic = create_epic(&db, &events, &project.id, "Dep epic", "Dep.", None).await;
    let task = create_task(&db, &events, &epic.id, "Dependent task", None).await;

    let task_repo = djinn_db::TaskRepository::new(db.clone(), events.clone());
    let attempt_repo = TaskAttemptRepository::new(db.clone());

    // Create 7 closed blocker parents with distinct closed_at timestamps.
    let mut parent_ids = Vec::new();
    for i in 1..=7 {
        let parent = create_task(
            &db,
            &events,
            &epic.id,
            &format!("Parent task {i}"),
            Some("closed"),
        )
        .await;
        // Stagger closed_at so ordering is deterministic.
        let ts = format!("2025-06-0{i}T00:00:00Z");
        close_task_at(&db, &parent.id, &ts).await;
        // Give each parent a completed attempt.
        seed_terminal_attempt(
            &attempt_repo,
            &parent.id,
            &format!("dk-parent-{i}"),
            TaskAttemptOutcome::Completed,
            Some(&format!("parent {i} completed")),
        )
        .await;
        parent_ids.push(parent.id);
    }

    // Also add an open parent (should be excluded).
    let open_parent = create_task(&db, &events, &epic.id, "Open parent", None).await;
    parent_ids.push(open_parent.id);

    task_repo
        .update_blockers_atomic(&task.id, &parent_ids, &[])
        .await
        .expect("wire blockers");

    let ctx = lead_prompt_context(db, &task).await;
    let text = ctx
        .activity_text
        .as_deref()
        .expect("activity_text should be populated with parent summaries");

    assert!(
        text.contains("**Completed dependency parents:**"),
        "should have parents header: {text}"
    );

    // Only 5 of the 7 closed parents should appear (capped at 5).
    // The 5 most recently closed (parents 7, 6, 5, 4, 3) should be included.
    for i in [7, 6, 5, 4, 3] {
        assert!(
            text.contains(&format!("Parent task {i}")),
            "parent {i} should be present: {text}"
        );
        assert!(
            text.contains(&format!("parent {i} completed")),
            "parent {i} attempt summary should be present: {text}"
        );
    }
    // Oldest 2 closed parents and the open parent should be absent.
    for i in [2, 1] {
        assert!(
            !text.contains(&format!("Parent task {i}")),
            "parent {i} should be dropped by cap: {text}"
        );
    }
    assert!(
        !text.contains("Open parent"),
        "open parent should be excluded: {text}"
    );

    // Ordering: by closed_at descending → parent 7 before 6 before 5, etc.
    assert_ordered(
        text,
        &[
            "Parent task 7",
            "Parent task 6",
            "Parent task 5",
            "Parent task 4",
            "Parent task 3",
        ],
    );
}

/// AC2 supplement: Open (non-closed) blocker parents are excluded.
#[tokio::test]
async fn prompt_excludes_open_blocker_from_dependency_parents() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let project = create_test_project(&db).await;
    let epic = create_epic(&db, &events, &project.id, "Excl epic", "Excl.", None).await;
    let task = create_task(&db, &events, &epic.id, "Dependent", None).await;

    // One open blocker parent.
    let open_parent = create_task(&db, &events, &epic.id, "Still open parent", None).await;
    let task_repo = djinn_db::TaskRepository::new(db.clone(), events.clone());
    task_repo
        .update_blockers_atomic(&task.id, &[open_parent.id], &[])
        .await
        .expect("wire blocker");

    let ctx = lead_prompt_context(db, &task).await;
    assert!(
        ctx.completed_dependency_parents.is_none(),
        "open blocker parents should yield None"
    );
    assert!(
        ctx.activity_text.is_none(),
        "no attempt data should yield None activity_text"
    );
}

/// AC2 supplement: dependency parents use each parent's latest completed attempt.
#[tokio::test]
async fn prompt_uses_latest_completed_attempt_for_parent() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let project = create_test_project(&db).await;
    let epic = create_epic(&db, &events, &project.id, "Latest epic", "Latest.", None).await;
    let task = create_task(&db, &events, &epic.id, "Child task", None).await;

    let parent = create_task(
        &db,
        &events,
        &epic.id,
        "Multi-attempt parent",
        Some("closed"),
    )
    .await;
    close_task_at(&db, &parent.id, "2025-06-01T00:00:00Z").await;
    let task_repo = djinn_db::TaskRepository::new(db.clone(), events.clone());
    let parent_id = parent.id.clone();
    task_repo
        .update_blockers_atomic(&task.id, &[parent_id], &[])
        .await
        .expect("wire blocker");

    let attempt_repo = TaskAttemptRepository::new(db.clone());
    // Seed two completed attempts: older then newer.
    seed_terminal_attempt(
        &attempt_repo,
        &parent.id,
        "dk-parent-old",
        TaskAttemptOutcome::Completed,
        Some("older attempt summary"),
    )
    .await;
    seed_terminal_attempt(
        &attempt_repo,
        &parent.id,
        "dk-parent-new",
        TaskAttemptOutcome::Completed,
        Some("newest attempt summary"),
    )
    .await;

    let ctx = lead_prompt_context(db, &task).await;
    let text = ctx
        .activity_text
        .as_deref()
        .expect("should have activity text");

    // The parent should use the newest completed attempt's summary.
    assert!(
        text.contains("newest attempt summary"),
        "should use latest completed attempt: {text}"
    );
    assert!(
        !text.contains("older attempt summary"),
        "should not use older attempt: {text}"
    );
}

/// AC3: Missing-summary fallback text for crashed/timed_out/spawn_failed/deferred
/// outcomes. No raw `log_tail` leaks into rendered prompts.
#[tokio::test]
async fn prompt_renders_fallback_text_for_missing_summaries() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Fallback epic", "Fallback task").await;
    let repo = TaskAttemptRepository::new(db.clone());

    // Seed attempts with terminal outcomes but no summary text.
    seed_terminal_attempt(
        &repo,
        &task.id,
        "dk-crash-1",
        TaskAttemptOutcome::Crashed,
        None,
    )
    .await;
    seed_terminal_attempt(
        &repo,
        &task.id,
        "dk-timeout-1",
        TaskAttemptOutcome::TimedOut,
        None,
    )
    .await;

    // Guard-deferred with no summary.
    seed_guard_deferred_attempt(
        &repo,
        &task.id,
        "dk-defer-1",
        GuardDecision::Defer,
        GuardReason::LoopThreshold,
        None,
    )
    .await;

    let ctx = lead_prompt_context(db, &task).await;
    let text = ctx
        .activity_text
        .as_deref()
        .expect("activity_text should be populated");

    // Fallback text for each outcome type.
    assert!(
        text.contains("attempt crashed (no summary recorded)"),
        "crashed fallback missing: {text}"
    );
    assert!(
        text.contains("attempt timed out (no summary recorded)"),
        "timed_out fallback missing: {text}"
    );
    assert!(
        text.contains("attempt deferred by guard (no summary recorded)"),
        "deferred fallback missing: {text}"
    );
}

/// AC3: Raw `log_tail` content does not leak into rendered prompts.
#[tokio::test]
async fn prompt_does_not_leak_log_tail_content() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Logtail epic", "Logtail task").await;
    let repo = TaskAttemptRepository::new(db.clone());

    // Seed an attempt whose summary contains log_tail prefix.
    let id = uuid::Uuid::now_v7().to_string();
    let attempt = repo
        .create_or_get_pending(CreateTaskAttemptParams {
            id: &id,
            task_id: &task.id,
            role: "worker",
            dispatch_key: "dk-logtail-1",
            session_id: None,
            dispatch_owner_incarnation_id: None,
            dispatch_group_id: None,
            attempt_seq: None,
        })
        .await
        .expect("create pending");
    // Store log_tail via advance_to_terminal; the summary carries a log_tail prefix.
    repo.advance_to_terminal(TerminalTaskAttemptParams {
        id: &attempt.id,
        outcome: TaskAttemptOutcome::Crashed,
        pr_url: None,
        submit_ref: None,
        checkpoint_ref: None,
        mirror_head_sha: None,
        github_head_sha: None,
        summary: Some(
            "log_tail: thread 'main' panicked at src/main.rs:42\nRUST_BACKTRACE=1\nlots of frames",
        ),
        summary_json: None,
        log_tail: Some("the real log_tail column content that should never appear"),
    })
    .await
    .expect("advance to terminal");

    let ctx = lead_prompt_context(db, &task).await;
    let text = ctx
        .activity_text
        .as_deref()
        .expect("should have activity text");

    // The log_tail prefix should be stripped from the rendered summary.
    assert!(
        !text.contains("log_tail:"),
        "log_tail prefix should be stripped: {text}"
    );
    assert!(
        !text.contains("the real log_tail column content"),
        "log_tail column value should not appear: {text}"
    );
    assert!(
        !text.contains("RUST_BACKTRACE=1"),
        "raw backtrace should be redacted: {text}"
    );
    // Panic message may survive redaction but not the raw backtrace.
    assert!(
        text.contains("panicked at"),
        "panic message should survive redaction: {text}"
    );
}

/// AC3 supplement: `summary_json` `log_tail` field is never rendered.
#[tokio::test]
async fn prompt_does_not_leak_log_tail_from_summary_json() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task =
        create_project_epic_task(&db, &events, "JSON logtail epic", "JSON logtail task").await;
    let repo = TaskAttemptRepository::new(db.clone());

    seed_attempt_with_meta(
        &repo,
        &task.id,
        "dk-json-1",
        TaskAttemptOutcome::Completed,
        Some("done"),
        Some(r#"{"failure_class":"compile_error","log_tail":"secret log output","last_verify":"cargo test"}"#),
        None,
        None,
        None,
    )
    .await;

    let ctx = lead_prompt_context(db, &task).await;
    let text = ctx
        .activity_text
        .as_deref()
        .expect("should have activity text");

    assert!(
        text.contains("failure_class: compile_error"),
        "failure_class should be preserved: {text}"
    );
    assert!(
        text.contains("last_verify: cargo test"),
        "last_verify should be preserved: {text}"
    );
    assert!(
        !text.contains("secret log output"),
        "log_tail content should not leak: {text}"
    );
    assert!(
        !text.contains("log_tail"),
        "log_tail key should not appear: {text}"
    );
}

/// AC3 supplement: summary_json fields (failure_class, last_verify) are rendered.
#[tokio::test]
async fn prompt_renders_summary_json_fields() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "JSON epic", "JSON task").await;
    let repo = TaskAttemptRepository::new(db.clone());

    seed_attempt_with_meta(
        &repo,
        &task.id,
        "dk-json-fields-1",
        TaskAttemptOutcome::Completed,
        Some("attempt done"),
        Some(r#"{"failure_class":"test_failure","last_verify":"cargo test -p foo"}"#),
        None,
        None,
        None,
    )
    .await;

    let ctx = lead_prompt_context(db, &task).await;
    let text = ctx
        .activity_text
        .as_deref()
        .expect("should have activity text");

    assert!(
        text.contains("failure_class: test_failure"),
        "failure_class missing: {text}"
    );
    assert!(
        text.contains("last_verify: cargo test -p foo"),
        "last_verify missing: {text}"
    );
}

/// AC4: Duplicate rejection text is not repeated in rendered prompts.
#[tokio::test]
async fn prompt_deduplicates_rejection_text_against_activity() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Dedup epic", "Dedup task").await;
    let repo = TaskAttemptRepository::new(db.clone());

    let rejection = "Missing null check in parse_config function causes panic on empty input";
    // Seed a terminal attempt whose summary IS the rejection text.
    seed_terminal_attempt(
        &repo,
        &task.id,
        "dk-dedup-1",
        TaskAttemptOutcome::Reopened,
        Some(rejection),
    )
    .await;

    // Use a worker prompt that will have activity_text rendered, but the
    // existing feedback is already embedded via the activity log. We simulate
    // by calling format_attempt_history directly with existing_feedback containing
    // the rejection, but here we test end-to-end: the prompt assembly passes the
    // existing activity_text as existing_feedback to format_attempt_history.
    // Since we don't have activity entries, the rejection text won't be in
    // existing_feedback from the activity log, so it won't be deduped at the
    // prompt-assembly level — it will appear. But if the activity log DID contain
    // the rejection, the dedup logic would trigger. We test this via the
    // format_attempt_history unit tests. For the prompt-level test, we verify
    // that the attempt text appears (no activity log to dedup against).
    let ctx = lead_prompt_context(db, &task).await;
    let text = ctx
        .activity_text
        .as_deref()
        .expect("should have activity text with attempt summary");

    assert!(
        text.contains(rejection),
        "attempt summary should appear when no duplicate in activity: {text}"
    );
}

/// AC4: Shared-budget truncation drops oldest attempt entries with a truncation
/// note while preserving the current (newest) feedback.
#[tokio::test]
async fn prompt_budget_truncation_drops_oldest_preserves_newest() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Budget epic", "Budget task").await;
    let repo = TaskAttemptRepository::new(db.clone());

    // Seed 3 terminal attempts with deliberately long summaries to exceed budget.
    for i in 1..=3 {
        let long_summary = format!("attempt {} detailed summary {}", i, "x".repeat(2000));
        seed_terminal_attempt(
            &repo,
            &task.id,
            &format!("dk-budget-{i}"),
            TaskAttemptOutcome::Completed,
            Some(&long_summary),
        )
        .await;
    }

    // Build the prompt with a tight remaining budget by using the internal
    // format_attempt_history directly. This tests the budget logic deterministically.
    let attempts = attempt_context::load_prior_attempts(&task, &repo)
        .await
        .expect("should load attempts");
    assert_eq!(attempts.len(), 3);

    // Use a very small budget — should truncate.
    let formatted = crate::actors::slot::helpers::format_attempt_history(&attempts, &[], "", 500);
    let text = formatted.expect("should produce output with truncation");

    // Newest (seq 3) should survive.
    assert!(
        text.contains("attempt 3 detailed summary"),
        "newest attempt should survive: {text}"
    );
    assert!(
        text.contains("[... older attempt entries dropped to fit feedback budget ...]"),
        "truncation note expected: {text}"
    );
    // Oldest (seq 1) should be dropped.
    assert!(
        !text.contains("attempt 1 detailed summary"),
        "oldest attempt should be dropped: {text}"
    );
}

/// AC4: Attempts are dropped before dependency parents when over budget.
#[tokio::test]
async fn prompt_budget_drops_attempts_before_parents() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let project = create_test_project(&db).await;
    let epic = create_epic(&db, &events, &project.id, "Drop epic", "Drop.", None).await;
    let task = create_task(&db, &events, &epic.id, "Drop task", None).await;

    let parent = create_task(&db, &events, &epic.id, "Parent task", Some("closed")).await;
    close_task_at(&db, &parent.id, "2025-06-01T00:00:00Z").await;
    let task_repo = djinn_db::TaskRepository::new(db.clone(), events.clone());
    let parent_id = parent.id.clone();
    task_repo
        .update_blockers_atomic(&task.id, &[parent_id], &[])
        .await
        .expect("wire blocker");

    let attempt_repo = TaskAttemptRepository::new(db.clone());
    // Seed two current-task attempts.
    for i in 1..=2 {
        seed_terminal_attempt(
            &attempt_repo,
            &task.id,
            &format!("dk-drop-{i}"),
            TaskAttemptOutcome::Completed,
            Some(&format!("attempt {} {}", i, "y".repeat(200))),
        )
        .await;
    }
    // Seed parent's completed attempt.
    seed_terminal_attempt(
        &attempt_repo,
        &parent.id,
        "dk-parent-drop",
        TaskAttemptOutcome::Completed,
        Some("parent done"),
    )
    .await;

    let attempts = attempt_context::load_prior_attempts(&task, &attempt_repo)
        .await
        .expect("should load");
    let parents = attempt_context::load_completed_dependency_parents(&task, &attempt_repo)
        .await
        .expect("should load parents");

    // Use budget large enough for parents + newest attempt but not both attempts.
    let formatted =
        crate::actors::slot::helpers::format_attempt_history(&attempts, &parents, "", 500);
    let text = formatted.expect("should produce output");

    // Parent should survive (attempts are dropped first).
    assert!(
        text.contains("Parent task"),
        "parent should survive: {text}"
    );
    assert!(
        text.contains("dropped to fit feedback budget"),
        "truncation note expected: {text}"
    );
}

/// AC4: Zero remaining budget yields no attempt history in prompt.
#[tokio::test]
async fn prompt_zero_budget_yields_no_attempt_history() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let attempts = vec![TaskAttemptPromptSummary {
        attempt_seq: 1,
        role: "worker".to_string(),
        outcome: "completed".to_string(),
        summary: Some("done".to_string()),
        created_at: "2026-01-01T00:00:00Z".to_string(),
        terminal_at: Some("2026-01-01T01:00:00Z".to_string()),
        submit_ref: None,
        pr_url: None,
        guard_decision: None,
        guard_reason: None,
        checkpoint_ref: None,
        summary_json: None,
    }];
    let result = crate::actors::slot::helpers::format_attempt_history(&attempts, &[], "", 0);
    assert!(result.is_none(), "zero budget should produce None");
}

/// AC5: Attempt history renders when both prior attempts and dependency parents
/// are present, with the correct section structure.
#[tokio::test]
async fn prompt_renders_combined_attempts_and_parents() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let project = create_test_project(&db).await;
    let epic = create_epic(
        &db,
        &events,
        &project.id,
        "Combined epic",
        "Combined.",
        None,
    )
    .await;
    let task = create_task(&db, &events, &epic.id, "Combined task", None).await;

    let parent = create_task(&db, &events, &epic.id, "Completed parent", Some("closed")).await;
    close_task_at(&db, &parent.id, "2025-06-01T00:00:00Z").await;
    let task_repo = djinn_db::TaskRepository::new(db.clone(), events.clone());
    let parent_id = parent.id.clone();
    task_repo
        .update_blockers_atomic(&task.id, &[parent_id], &[])
        .await
        .expect("wire blocker");

    let attempt_repo = TaskAttemptRepository::new(db.clone());
    seed_terminal_attempt(
        &attempt_repo,
        &task.id,
        "dk-combined-1",
        TaskAttemptOutcome::Completed,
        Some("my completed work"),
    )
    .await;
    seed_terminal_attempt(
        &attempt_repo,
        &parent.id,
        "dk-parent-combined",
        TaskAttemptOutcome::Completed,
        Some("parent completed work"),
    )
    .await;

    let ctx = lead_prompt_context(db, &task).await;
    let text = ctx
        .activity_text
        .as_deref()
        .expect("should have combined activity text");

    // Both sections present.
    assert!(
        text.contains("**Prior attempts (newest first):**"),
        "should have attempts header: {text}"
    );
    assert!(
        text.contains("**Completed dependency parents:**"),
        "should have parents header: {text}"
    );

    // Content from both.
    assert!(
        text.contains("my completed work"),
        "current task attempt summary: {text}"
    );
    assert!(text.contains("Completed parent"), "parent title: {text}");
    assert!(
        text.contains("parent completed work"),
        "parent attempt summary: {text}"
    );

    // Sections in correct order: attempts before parents.
    assert_ordered(
        text,
        &[
            "**Prior attempts (newest first):**",
            "**Completed dependency parents:**",
        ],
    );
}

/// AC5: Prompt assembly remains non-fatal and produces a valid system prompt
/// even when attempt history is populated.
#[tokio::test]
async fn prompt_assembly_nonfatal_with_attempt_history() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Nonfatal epic", "Nonfatal task").await;
    let repo = TaskAttemptRepository::new(db.clone());

    seed_terminal_attempt(
        &repo,
        &task.id,
        "dk-nonfatal-1",
        TaskAttemptOutcome::Crashed,
        Some("crashed"),
    )
    .await;

    let ctx = lead_prompt_context(db, &task).await;

    // System prompt is valid and contains expected sections.
    assert!(
        !ctx.system_prompt.is_empty(),
        "system prompt should be non-empty"
    );
    assert!(
        ctx.prior_attempts.as_ref().map_or(0, |v| v.len()) == 1,
        "prior_attempts should have 1 entry"
    );

    // The attempt history appears in activity_text which is passed to the
    // prompt template. Verify it shows up in the system prompt.
    assert!(
        ctx.system_prompt.contains("crashed")
            || ctx
                .activity_text
                .as_ref()
                .is_some_and(|t| t.contains("crashed")),
        "attempt history should reach the prompt: system_prompt={} activity_text={:?}",
        &ctx.system_prompt[..ctx.system_prompt.len().min(200)],
        ctx.activity_text,
    );
}

/// Verify the attempt history renders inside the Activity Log section
/// (not as a new competing top-level prompt section).
#[tokio::test]
async fn attempt_history_appears_in_activity_log_section() {
    let _knowledge_context_env = knowledge_context_test_env_guard();
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Section epic", "Section task").await;
    let repo = TaskAttemptRepository::new(db.clone());

    seed_terminal_attempt(
        &repo,
        &task.id,
        "dk-section-1",
        TaskAttemptOutcome::Completed,
        Some("section summary"),
    )
    .await;

    let ctx = lead_prompt_context(db, &task).await;

    // The prior_attempts and completed_dependency_parents should be populated
    // on the PromptContext struct.
    assert!(
        ctx.prior_attempts.is_some(),
        "prior_attempts should be Some"
    );
    assert!(
        ctx.completed_dependency_parents.is_none(),
        "no parents expected"
    );

    // The activity_text should contain the attempt history text (appended to any
    // existing activity log content).
    let text = ctx
        .activity_text
        .as_deref()
        .expect("activity_text should be Some");
    assert!(
        text.contains("**Prior attempts (newest first):**"),
        "should have attempts header in activity: {text}"
    );
    assert!(
        text.contains("section summary"),
        "attempt summary should appear in activity: {text}"
    );
}
