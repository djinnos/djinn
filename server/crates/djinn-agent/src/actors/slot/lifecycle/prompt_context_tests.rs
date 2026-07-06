use super::*;

use djinn_core::events::EventBus;
use djinn_core::models::ActivityEntry;
use djinn_db::{Database, EpicRepository, ProposalCreateInput, ProposalRepository};
use tokio_util::sync::CancellationToken;

use crate::roles::{LeadRole, WorkerRole};
use crate::test_helpers::{agent_context_from_db, create_test_project};

use super::test_support::{
    assemble_for_role, assemble_for_role_with_mcp_instructions, assemble_for_role_with_resume,
    assert_contains_all, assert_ordered, create_epic, create_project_epic_task, create_task,
};

async fn lead_prompt_context(db: Database, task: &Task) -> PromptContext {
    let role = LeadRole;
    assemble_for_role(db, task, &role, None, "", None, &[], &[]).await
}

#[tokio::test]
async fn epic_context_omits_sections_when_no_blockers_or_proposal() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Standalone epic", "Standalone task").await;
    let epic_context = lead_prompt_context(db, &task)
        .await
        .epic_context
        .expect("lead prompt context includes epic context");
    for section in ["### Blocking Epics", "### Proposal Sibling Epics"] {
        assert!(
            !epic_context.contains(section),
            "unexpected section {section}"
        );
    }
}

#[tokio::test]
async fn missing_activity_yields_none_activity_text() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "No-activity epic", "No-activity task").await;
    assert!(
        lead_prompt_context(db, &task).await.activity_text.is_none(),
        "task with no activity entries should yield None activity_text"
    );
}

#[tokio::test]
async fn conflict_context_formats_files_and_preserves_prompt_fields() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Conflict epic", "Conflict task").await;
    let conflict = MergeConflictMetadata {
        conflicting_files: vec!["src/main.rs".to_string(), "Cargo.toml".to_string()],
        base_branch: "feature-branch".to_string(),
        merge_target: "main".to_string(),
    };
    let role = LeadRole;
    let ctx = assemble_for_role(db, &task, &role, Some(&conflict), "", None, &[], &[]).await;
    assert_contains_all(
        ctx.conflict_files
            .as_deref()
            .expect("conflict_files should be Some"),
        &["- src/main.rs", "- Cargo.toml"],
    );
    assert!(!ctx.base_system_prompt.is_empty());
    assert!(!ctx.system_prompt.is_empty());
}

#[tokio::test]
async fn prompt_sections_append_in_canonical_order() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task =
        create_project_epic_task(&db, &events, "Prompt section epic", "Prompt section task").await;
    let role = LeadRole;
    let skills = vec![
        skill("alpha-skill", "First skill", "Alpha body.", true),
        skill("beta-skill", "Second skill", "Beta body.", false),
    ];
    let sources = vec![
        source("repo-a", "Repository A"),
        source("repo-b", "Repository B"),
    ];
    let ctx = assemble_for_role(
        db,
        &task,
        &role,
        None,
        "Custom extension text.",
        None,
        &skills,
        &sources,
    )
    .await;
    assert_contains_all(
        &ctx.system_prompt,
        &[
            "Custom extension text.",
            "## Available Skills",
            "**alpha-skill**",
            "**beta-skill**",
            "## Related repositories (read-only)",
            "repo-a",
            "repo-b",
        ],
    );
    assert_ordered(
        &ctx.system_prompt,
        &[
            "Custom extension text.",
            "## Available Skills",
            "## Related repositories (read-only)",
        ],
    );
}

#[test]
fn format_conflict_files_cases() {
    assert!(format_conflict_files(None).is_none());
    let ctx = MergeConflictMetadata {
        conflicting_files: vec![
            "src/main.rs".to_string(),
            "Cargo.toml".to_string(),
            "tests/integration.rs".to_string(),
        ],
        base_branch: "feature".to_string(),
        merge_target: "main".to_string(),
    };
    let result = format_conflict_files(Some(&ctx)).expect("should produce Some");
    assert_contains_all(
        &result,
        &[
            "- src/main.rs",
            "- Cargo.toml",
            "- tests/integration.rs",
            "\n",
        ],
    );
    let empty = MergeConflictMetadata {
        conflicting_files: vec![],
        base_branch: "feature".to_string(),
        merge_target: "main".to_string(),
    };
    assert_eq!(format_conflict_files(Some(&empty)).as_deref(), Some(""));
}

#[test]
fn format_activity_text_absence_and_comment_counts() {
    assert!(format_activity_text(&None, 3).is_none());
    assert!(format_activity_text(&Some(vec![]), 3).is_none());
    let entries = Some(vec![
        activity("1", "lead", "comment", r#"{"body":"Good work"}"#, 0),
        activity("2", "reviewer", "comment", r#"{"body":"Looks good"}"#, 1),
        activity("3", "lead", "comment", r#"{"body":"Approved"}"#, 2),
        activity("4", "system", "status_changed", "{}", 3),
    ]);
    let result = format_activity_text(&entries, 3).expect("should produce Some");
    assert_contains_all(&result, &["Activity totals:", "1 reviewer", "2 lead"]);
    assert!(
        !result.contains("system"),
        "non-comment events should not be counted: {result}"
    );
}

#[test]
fn apply_prompt_sections_cases() {
    let empty_instructions = std::collections::BTreeMap::new();
    assert_eq!(
        apply_prompt_sections("Base prompt.", "", None, &[], &[], &empty_instructions),
        "Base prompt."
    );
    let result = apply_prompt_sections(
        "Base system prompt content.",
        "Custom extension text.",
        None,
        &[skill("test-skill", "A test skill", "Skill body.", false)],
        &[source("sibling-repo", "Sibling")],
        &empty_instructions,
    );
    assert_contains_all(
        &result,
        &[
            "Base system prompt content.",
            "Custom extension text.",
            "## Available Skills",
            "## Related repositories (read-only)",
            "sibling-repo",
        ],
    );
    assert_ordered(
        &result,
        &[
            "Custom extension text.",
            "## Available Skills",
            "## Related repositories (read-only)",
        ],
    );
}

struct NoEpicRole;

impl std::fmt::Debug for NoEpicRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NoEpicRole").finish()
    }
}

impl AgentRole for NoEpicRole {
    fn config(&self) -> &crate::roles::RoleConfig {
        &NO_EPIC_CONFIG
    }
    fn render_prompt(&self, task: &Task, ctx: &TaskContext) -> String {
        crate::prompts::render_prompt_for_role(self.config(), task, ctx)
    }
    fn needs_epic_context(&self) -> bool {
        false
    }
}

static NO_EPIC_CONFIG: crate::roles::RoleConfig = crate::roles::RoleConfig {
    name: "no-epic-test",
    display_name: "No Epic Test",
    dispatch_role: "no-epic-test",
    initial_message: "Test role without epic context.",
    finalize_tool_names: &["submit_work"],
    mode_section: None,
};

#[tokio::test]
async fn epic_context_not_loaded_when_role_does_not_need_it() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Epic", "Task").await;
    let app_state = agent_context_from_db(db.clone(), CancellationToken::new());
    assert!(load_epic_context(&task, false, &app_state).await.is_none());
    let role = NoEpicRole;
    let ctx = assemble_for_role(db, &task, &role, None, "", None, &[], &[]).await;
    assert!(ctx.epic_context.is_none());
    assert!(ctx.knowledge_context.is_none());
}

#[tokio::test]
async fn load_epic_context_returns_context_when_epic_exists() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Test Epic Title", "Test task").await;
    let app_state = agent_context_from_db(db, CancellationToken::new());
    let result = load_epic_context(&task, true, &app_state)
        .await
        .expect("should return Some");
    assert_contains_all(
        &result,
        &[
            "Test Epic Title",
            "Test epic",
            "### Sibling Tasks",
            "memory_read",
        ],
    );
}

#[tokio::test]
async fn load_knowledge_context_returns_none_when_no_notes() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task =
        create_project_epic_task(&db, &events, "Knowledge test epic", "Knowledge task").await;
    let app_state = agent_context_from_db(db, CancellationToken::new());
    assert!(
        load_knowledge_context(&task, None, &app_state)
            .await
            .is_none()
    );
}

#[tokio::test]
async fn load_epic_context_includes_blocker_and_sibling_sections_in_order() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let project = create_test_project(&db).await;
    let epic_repo = EpicRepository::new(db.clone(), events.clone());
    let proposal_repo = ProposalRepository::new(db.clone(), events.clone());
    let subject_epic =
        create_epic(&db, &events, &project.id, "Subject epic", "Subject.", None).await;
    let blocking_epic = create_epic(
        &db,
        &events,
        &project.id,
        "Blocking epic",
        "Blocks the subject.",
        Some("closed"),
    )
    .await;
    create_task(
        &db,
        &events,
        &blocking_epic.id,
        "Delivered blocker task",
        Some("closed"),
    )
    .await;
    epic_repo
        .update_blockers_atomic(
            &subject_epic.id,
            std::slice::from_ref(&blocking_epic.id),
            &[],
        )
        .await
        .expect("wire blocker");
    let sibling_epic = create_epic(
        &db,
        &events,
        &project.id,
        "Proposal sibling epic",
        "Sibling.",
        None,
    )
    .await;
    let proposal = proposal_repo
        .create(ProposalCreateInput {
            title: "Test proposal",
            body: "body",
            acceptance_criteria: None,
            status: Some("building"),
            body_format: None,
        })
        .await
        .expect("create proposal");
    for epic_id in [&subject_epic.id, &sibling_epic.id] {
        proposal_repo
            .link_epic(&proposal.id, epic_id, &project.id)
            .await
            .expect("link epic");
    }
    let task = create_task(&db, &events, &subject_epic.id, "Subject task", None).await;
    let app_state = agent_context_from_db(db, CancellationToken::new());
    let result = load_epic_context(&task, true, &app_state)
        .await
        .expect("should return Some");
    assert_contains_all(
        &result,
        &[
            "### Sibling Tasks",
            "### Blocking Epics",
            "Delivered blocker task",
            "### Proposal Sibling Epics",
            "Proposal sibling epic",
        ],
    );
    assert_ordered(
        &result,
        &[
            "### Sibling Tasks",
            "### Blocking Epics",
            "### Proposal Sibling Epics",
        ],
    );
}

fn skill(name: &str, description: &str, content: &str, required: bool) -> ResolvedSkill {
    ResolvedSkill {
        name: name.to_string(),
        description: description.to_string(),
        content: content.to_string(),
        required,
        trust_level: "project".to_string(),
        recommended_for_roles: vec![],
        tags: vec![],
    }
}

fn source(slug: &str, name: &str) -> ReadSourceInfo {
    ReadSourceInfo {
        slug: slug.to_string(),
        name: name.to_string(),
    }
}

fn activity(
    id: &str,
    actor_role: &str,
    event_type: &str,
    payload: &str,
    hour: u8,
) -> ActivityEntry {
    ActivityEntry {
        id: id.to_string(),
        task_id: Some("t1".to_string()),
        actor_id: actor_role.to_string(),
        actor_role: actor_role.to_string(),
        event_type: event_type.to_string(),
        payload: payload.to_string(),
        created_at: format!("2025-01-01T0{hour}:00:00Z"),
    }
}

fn resume_metadata_with_checkpoint() -> djinn_runtime::ResumeLifecycleMetadata {
    djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        checkpoint_id: Some("ckpt-1".to_string()),
        commit_sha: Some("abc123def456".to_string()),
        selection_reason: Some(djinn_runtime::ResumeSelectionReason::LatestSafeCheckpoint),
        source_kind: Some(djinn_runtime::ResumeSourceKind::TaskBranchCheckpoint),
        target_ref: Some("refs/heads/task/test".to_string()),
        prior_session_lineage: Some("session-prior-001".to_string()),
        previous_model: Some("anthropic/claude-opus-4.7".to_string()),
        new_model: Some("openai/gpt-4.1".to_string()),
        failover_reason: Some("no_durable_progress_streak".to_string()),
        last_durable_progress_summary: Some("Implemented core feature".to_string()),
        verification_command: Some("cargo test -p djinn-agent".to_string()),
        ..Default::default()
    }
}

fn resume_metadata_with_auto_submit() -> djinn_runtime::ResumeLifecycleMetadata {
    djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        selection_reason: Some(djinn_runtime::ResumeSelectionReason::AutoSubmitAccepted),
        source_kind: Some(djinn_runtime::ResumeSourceKind::AutoSubmit),
        target_ref: Some("refs/heads/task/test".to_string()),
        submit_or_review_id: Some("review-7".to_string()),
        prior_session_lineage: Some("session-prior-002".to_string()),
        ..Default::default()
    }
}

#[tokio::test]
async fn worker_resume_note_injected_for_worker_role() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Resume epic", "Resume task").await;
    let role = WorkerRole;
    let metadata = resume_metadata_with_checkpoint();
    let note = build_worker_resume_note(role.config().name, Some(&metadata));
    assert!(note.is_some(), "worker role should receive resume note");
    let ctx = assemble_for_role_with_resume(db, &task, &role, note.as_deref()).await;
    assert!(ctx.worker_resume_note.is_some());
    assert_contains_all(
        ctx.worker_resume_note.as_deref().unwrap(),
        &[
            "Resuming from prior session",
            "session-prior-001",
            "abc123def456",
            "claude-opus-4.7",
            "gpt-4.1",
            "no_durable_progress_streak",
            "no-progress checkpoint",
            "task-branch checkpoint",
            "refs/heads/task/test",
            "Implemented core feature",
            "cargo test -p djinn-agent",
        ],
    );
    assert!(ctx.system_prompt.contains("## Resume Context"));
    assert!(ctx.system_prompt.contains("Resuming from prior session"));
}

#[tokio::test]
async fn worker_resume_note_included_for_auto_submit_source() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let _task =
        create_project_epic_task(&db, &events, "Auto-submit epic", "Auto-submit task").await;
    let role = WorkerRole;
    let metadata = resume_metadata_with_auto_submit();
    let note = build_worker_resume_note(role.config().name, Some(&metadata));
    assert!(note.is_some());
    let note_text = note.unwrap();
    assert_contains_all(
        &note_text,
        &["session-prior-002", "review-7", "auto-submit accepted"],
    );
    assert!(!note_text.contains("checkpoint"));
}

#[test]
fn worker_resume_note_absent_cases() {
    let metadata = resume_metadata_with_checkpoint();
    for role_name in ["lead", "reviewer", "planner", "architect"] {
        assert!(build_worker_resume_note(role_name, Some(&metadata)).is_none());
    }
    let not_considered = djinn_runtime::ResumeLifecycleMetadata {
        considered: false,
        ..resume_metadata_with_checkpoint()
    };
    assert!(build_worker_resume_note("worker", Some(&not_considered)).is_none());
    let no_fields = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        commit_sha: None,
        submit_or_review_id: None,
        prior_session_lineage: None,
        ..Default::default()
    };
    assert!(build_worker_resume_note("worker", Some(&no_fields)).is_none());
    assert!(build_worker_resume_note("worker", None).is_none());
}

#[test]
fn role_receives_worker_resume_check() {
    assert!(role_receives_worker_resume("worker"));
    for non_worker in [
        "lead",
        "reviewer",
        "planner",
        "architect",
        "advocate",
        "adversary",
        "judge",
    ] {
        assert!(!role_receives_worker_resume(non_worker));
    }
}

#[test]
fn worker_resume_note_truncates_long_progress_summary() {
    let long_summary = "x".repeat(200);
    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        commit_sha: Some("sha1".to_string()),
        prior_session_lineage: Some("s1".to_string()),
        last_durable_progress_summary: Some(long_summary),
        ..Default::default()
    };
    let note = build_worker_resume_note("worker", Some(&metadata)).unwrap();
    assert!(note.contains('…'));
    assert!(!note.contains(&"x".repeat(200)));
}

#[test]
fn worker_resume_note_minimal_fields() {
    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        prior_session_lineage: Some("sess-1".to_string()),
        ..Default::default()
    };
    let note = build_worker_resume_note("worker", Some(&metadata)).unwrap();
    assert!(note.contains("sess-1"));
}

#[test]
fn worker_resume_note_includes_failover_context() {
    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        prior_session_lineage: Some("session-prev".to_string()),
        previous_model: Some("anthropic/claude-opus-4.7".to_string()),
        new_model: Some("openai/gpt-4.1".to_string()),
        failover_reason: Some("provider_health_degraded".to_string()),
        source_kind: Some(djinn_runtime::ResumeSourceKind::CleanTaskBranch),
        target_ref: Some("refs/heads/task/test".to_string()),
        ..Default::default()
    };
    let note = build_worker_resume_note("worker", Some(&metadata)).unwrap();
    assert_contains_all(
        &note,
        &[
            "session-prev",
            "claude-opus-4.7",
            "gpt-4.1",
            "provider_health_degraded",
            "clean task branch",
            "refs/heads/task/test",
        ],
    );
}

#[test]
fn worker_resume_note_failover_only_produces_note() {
    // When no checkpoint/submit/prior session exists but failover context
    // is present, the note should still be produced.
    let metadata = djinn_runtime::ResumeLifecycleMetadata {
        considered: true,
        new_model: Some("openai/gpt-4.1".to_string()),
        failover_reason: Some("no_durable_progress_streak".to_string()),
        ..Default::default()
    };
    let note = build_worker_resume_note("worker", Some(&metadata)).unwrap();
    assert_contains_all(
        &note,
        &[
            "Resuming from prior session",
            "gpt-4.1",
            "no_durable_progress_streak",
        ],
    );
}

// ── MCP server instructions prompt tests ────────────────────────────

#[tokio::test]
async fn empty_mcp_instructions_omits_section_from_prompt() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "No MCP epic", "No MCP task").await;
    let instructions = std::collections::BTreeMap::new();
    let ctx = assemble_for_role_with_mcp_instructions(db, &task, &LeadRole, &instructions).await;
    assert!(
        !ctx.system_prompt.contains("MCP Server Instructions"),
        "empty instructions should not produce an MCP section"
    );
}

#[tokio::test]
async fn single_server_instructions_rendered_in_prompt() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "MCP epic", "MCP task").await;
    let mut instructions = std::collections::BTreeMap::new();
    instructions.insert(
        "search-server".to_string(),
        "Use web_search for live information.".to_string(),
    );
    let ctx = assemble_for_role_with_mcp_instructions(db, &task, &LeadRole, &instructions).await;
    assert_contains_all(
        &ctx.system_prompt,
        &[
            "## MCP Server Instructions",
            "### search-server",
            "Use web_search for live information.",
        ],
    );
}

#[tokio::test]
async fn multiple_servers_rendered_in_deterministic_name_order() {
    let db = Database::ephemeral().await.expect("create ephemeral db");
    let events = EventBus::noop();
    let task = create_project_epic_task(&db, &events, "Multi MCP epic", "Multi MCP task").await;
    let mut instructions = std::collections::BTreeMap::new();
    // Insert in reverse-alphabetical order; BTreeMap sorts by key.
    instructions.insert(
        "zebra-server".to_string(),
        "Zebra instructions.".to_string(),
    );
    instructions.insert(
        "alpha-server".to_string(),
        "Alpha instructions.".to_string(),
    );
    instructions.insert(
        "middle-server".to_string(),
        "Middle instructions.".to_string(),
    );
    let ctx = assemble_for_role_with_mcp_instructions(db, &task, &LeadRole, &instructions).await;
    assert_contains_all(
        &ctx.system_prompt,
        &[
            "## MCP Server Instructions",
            "### alpha-server",
            "Alpha instructions.",
            "### middle-server",
            "Middle instructions.",
            "### zebra-server",
            "Zebra instructions.",
        ],
    );
    assert_ordered(
        &ctx.system_prompt,
        &["### alpha-server", "### middle-server", "### zebra-server"],
    );
}

#[test]
fn format_mcp_instructions_omits_empty_map() {
    let instructions = std::collections::BTreeMap::new();
    assert!(
        super::format_mcp_instructions(&instructions).is_none(),
        "empty map should produce None"
    );
}

#[test]
fn format_mcp_instructions_renders_sorted_subsections() {
    let mut instructions = std::collections::BTreeMap::new();
    instructions.insert("beta".to_string(), "Beta instructions.".to_string());
    instructions.insert("alpha".to_string(), "Alpha instructions.".to_string());
    let result = super::format_mcp_instructions(&instructions).expect("should produce Some");
    assert_contains_all(
        &result,
        &[
            "## MCP Server Instructions",
            "### alpha",
            "Alpha instructions.",
            "### beta",
            "Beta instructions.",
        ],
    );
    assert_ordered(&result, &["### alpha", "### beta"]);
}
