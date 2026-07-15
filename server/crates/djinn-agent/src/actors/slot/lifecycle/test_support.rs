use super::*;

use djinn_core::events::EventBus;
use djinn_core::extension_diagnostics::ExtensionLoadDiagnosticV1;
use djinn_core::models::{Epic, Task};
use djinn_db::{Database, EpicCreateInput, EpicRepository, TaskRepository};
use tokio_util::sync::CancellationToken;

use crate::roles::AgentRole;
use crate::test_helpers::{
    agent_context_from_db, create_test_project, create_test_user, test_tempdir,
};

pub(crate) async fn create_epic(
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

pub(crate) async fn create_task(
    db: &Database,
    events: &EventBus,
    epic_id: &str,
    title: &str,
    status: Option<&str>,
) -> Task {
    let creator_id = create_test_user(db).await;
    djinn_core::auth_context::SESSION_USER_ID
        .scope(Some(creator_id), async {
            TaskRepository::new(db.clone(), events.clone())
                .create(
                    epic_id,
                    title,
                    "description",
                    "design",
                    "task",
                    1,
                    "test-owner",
                    status,
                )
                .await
                .expect("create task")
        })
        .await
}

pub(crate) async fn create_project_epic_task(
    db: &Database,
    events: &EventBus,
    epic_title: &str,
    task_title: &str,
) -> Task {
    let project = create_test_project(db).await;
    let epic = create_epic(db, events, &project.id, epic_title, "Test epic.", None).await;
    create_task(db, events, &epic.id, task_title, None).await
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn assemble_for_role(
    db: Database,
    task: &Task,
    role: &dyn AgentRole,
    conflict_ctx: Option<&MergeConflictMetadata>,
    system_prompt_extensions: &str,
    resolved_skills: &[ResolvedSkill],
    read_sources: &[ReadSourceInfo],
) -> PromptContext {
    assemble_for_role_with_extension_diagnostics(
        db,
        task,
        role,
        conflict_ctx,
        system_prompt_extensions,
        resolved_skills,
        read_sources,
        &[],
    )
    .await
}

/// Assemble the live prompt path with canonical persisted diagnostic rows.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn assemble_for_role_with_extension_diagnostics(
    db: Database,
    task: &Task,
    role: &dyn AgentRole,
    conflict_ctx: Option<&MergeConflictMetadata>,
    system_prompt_extensions: &str,
    resolved_skills: &[ResolvedSkill],
    read_sources: &[ReadSourceInfo],
    extension_diagnostics: &[ExtensionLoadDiagnosticV1],
) -> PromptContext {
    let app_state = agent_context_from_db(db, CancellationToken::new());
    let worktree = test_tempdir("prompt-context-worktree-");
    assemble_prompt_context(PromptContextInputs {
        task,
        runtime_role: role,
        role_for_epic_check: role,
        project_path: "/workspace/test-project",
        worktree_path: worktree.path(),
        conflict_ctx,
        merge_validation_ctx: None,
        prompt_setup_commands: None,
        system_prompt_extensions,
        resolved_skills,
        app_state: &app_state,
        read_sources,
        worker_resume_note: None,
        arbiter_directive: None,
        mcp_server_instructions: &std::collections::BTreeMap::new(),
        extension_diagnostics,
        memory_intent_planner: None,
    })
    .await
}

/// Variant of [`assemble_for_role`] that accepts MCP server instructions,
/// used by prompt-instructions tests.
#[allow(dead_code)]
pub(crate) async fn assemble_for_role_with_mcp_instructions(
    db: Database,
    task: &Task,
    role: &dyn AgentRole,
    mcp_server_instructions: &std::collections::BTreeMap<String, String>,
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
        resolved_skills: &[],
        app_state: &app_state,
        read_sources: &[],
        worker_resume_note: None,
        arbiter_directive: None,
        mcp_server_instructions,
        extension_diagnostics: &[],
        memory_intent_planner: None,
    })
    .await
}

/// Variant of [`assemble_for_role`] that accepts a worker resume note,
/// used by resume-context tests.
pub(crate) async fn assemble_for_role_with_resume(
    db: Database,
    task: &Task,
    role: &dyn AgentRole,
    worker_resume_note: Option<&str>,
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
        resolved_skills: &[],
        app_state: &app_state,
        read_sources: &[],
        worker_resume_note,
        arbiter_directive: None,
        mcp_server_instructions: &std::collections::BTreeMap::new(),
        extension_diagnostics: &[],
        memory_intent_planner: None,
    })
    .await
}

pub(crate) fn task_with_ci(
    ci_status: &str,
    ci_head_sha: Option<&str>,
    ci_pr_number: Option<i64>,
    ci_blocking_checks: &str,
    ci_failure_fingerprint: Option<&str>,
    ci_last_remediation_base_sha: Option<&str>,
) -> Task {
    Task {
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

pub(crate) fn assert_contains_all(haystack: &str, needles: &[&str]) {
    for needle in needles {
        assert!(
            haystack.contains(needle),
            "missing {needle:?} in {haystack}"
        );
    }
}

pub(crate) fn assert_ordered(haystack: &str, needles: &[&str]) {
    let mut previous = 0;
    for needle in needles {
        let pos = haystack[previous..]
            .find(needle)
            .map(|pos| previous + pos)
            .unwrap_or_else(|| panic!("missing ordered marker {needle:?} in {haystack}"));
        assert!(
            pos >= previous,
            "{needle:?} appeared out of order in {haystack}"
        );
        previous = pos;
    }
}
